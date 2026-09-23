use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The local-config key and directory prefix reclamation records use, mirrored
/// from `src/app/reclamation.rs` so a rename there fails these tests loudly.
const RECLAMATION_KEY: &str = "perch.reclamation.worktree";
const TRASH_PREFIX: &str = ".perch-trash.";

/// Serializes tests that mutate process cwd while calling library functions.
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Locks `CWD_LOCK`, switches process cwd, and restores the previous cwd on
/// drop — even on panic. Without this, a panicking test would leave cwd at a
/// deleted `TempDir` and cascade failures into unrelated tests.
struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    original: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

fn cwd_at(path: &Path) -> CwdGuard {
    // Poisoning is safe to recover from: the guard always restores cwd, so
    // the mutex's protected state is in fact consistent.
    let lock = CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(path).unwrap();
    CwdGuard {
        _lock: lock,
        original,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Hooks come from git config, which layers in the developer's global file, so
/// suppress them everywhere: a machine with `perch.hook.created` set would
/// otherwise run it throughout the suite. The hook tests use [`perch_hooked`].
fn perch_args(dir: &Path, args: &[&str]) -> Output {
    perch_command(dir, args)
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run perch")
}

/// Like [`perch_args`], but with hooks left on — for the tests that configure
/// one in the repo under test.
fn perch_hooked(dir: &Path, args: &[&str]) -> Output {
    perch_command(dir, args)
        .env_remove("PERCH_NO_HOOKS")
        .output()
        .expect("failed to run perch")
}

fn perch_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_perch"));
    // A transport program in the developer's own environment would make every
    // worktree fetch for itself, and the tests that count fetches say so. So
    // would a relative or empty `PATH` entry, or a relative `GIT_EXEC_PATH`.
    cmd.args(args)
        .current_dir(dir)
        .env_remove("GIT_PROXY_COMMAND")
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND");
    cmd
}

fn perch(dir: &Path, branch: &str) -> Output {
    perch_args(dir, &[branch])
}

fn reclamation_record(state: &str, original: &Path, trash: &Path) -> String {
    fn hex(path: &Path) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let bytes = path.to_str().unwrap().as_bytes();
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    format!("{state}:{}:{}", hex(original), hex(trash))
}

/// Point a bare repo's HEAD at `main`.
///
/// `git init --bare` derives HEAD from the host's `init.defaultBranch`, so on a
/// machine that still defaults to `master` the bare ends up with a HEAD that
/// names a ref the tests never create. Cloning it then checks out nothing —
/// "remote HEAD refers to nonexistent ref" — and a later `push origin main`
/// fails with "src refspec main does not match any". Pin it so the tests don't
/// depend on the developer's git config.
fn pin_default_branch(bare: &Path) {
    git(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

/// Like `setup`, but places the working clone inside a parent `TempDir` so
/// worktrees created at `<parent>/worktrees/<repo>/...` land in cleanable
/// space. Returns `(bare, parent, work_path)`.
fn setup_with_parent() -> (TempDir, TempDir, PathBuf) {
    let bare = TempDir::new().unwrap();
    let parent = TempDir::new().unwrap();
    let work = parent.path().join("repo");
    fs::create_dir(&work).unwrap();

    git(bare.path(), &["init", "--bare"]);
    pin_default_branch(bare.path());

    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "test"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    git(
        &work,
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );

    fs::write(work.join("file.txt"), "hello\n").unwrap();
    git(&work, &["add", "file.txt"]);
    git(&work, &["commit", "-m", "initial"]);
    git(&work, &["push", "-u", "origin", "main"]);
    git(&work, &["remote", "set-head", "origin", "main"]);

    (bare, parent, work)
}

/// Create a bare "remote" and a working clone with one commit on `main`.
fn setup() -> (TempDir, TempDir) {
    setup_with_remote("origin")
}

fn setup_with_remote(remote: &str) -> (TempDir, TempDir) {
    let bare = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    git(bare.path(), &["init", "--bare"]);
    pin_default_branch(bare.path());

    git(work.path(), &["init", "-b", "main"]);
    // Library code under test spawns its own `git` subprocesses without our
    // helper's GIT_*_NAME/EMAIL env, so commits it makes (e.g. during rebase)
    // need identity from .git/config — otherwise CI hosts without a global
    // gitconfig fail with "empty ident name".
    git(work.path(), &["config", "user.name", "test"]);
    git(work.path(), &["config", "user.email", "test@example.com"]);
    git(
        work.path(),
        &["remote", "add", remote, bare.path().to_str().unwrap()],
    );

    fs::write(work.path().join("file.txt"), "hello\n").unwrap();
    git(work.path(), &["add", "file.txt"]);
    git(work.path(), &["commit", "-m", "initial"]);
    git(work.path(), &["push", "-u", remote, "main"]);
    // `git clone` writes `refs/remotes/<remote>/HEAD`, but `init` + `remote add`
    // + `push` does not. Staleness is judged against the default branch, so a
    // setup without it wouldn't resemble any real clone.
    git(work.path(), &["remote", "set-head", remote, "main"]);

    (bare, work)
}

/// Push a commit to `<remote>/main` that the working tree doesn't have.
/// Works by committing locally, pushing, then rewinding.
fn push_upstream_change(work: &Path, file: &str, content: &str, msg: &str) {
    push_upstream_change_to(work, "origin", file, content, msg);
}

fn push_upstream_change_to(work: &Path, remote: &str, file: &str, content: &str, msg: &str) {
    fs::write(work.join(file), content).unwrap();
    git(work, &["add", file]);
    git(work, &["commit", "-m", msg]);
    git(work, &["push", remote, "main"]);
    git(work, &["reset", "--hard", "HEAD~1"]);
}

fn clone_bare(bare: &Path) -> TempDir {
    let dir = TempDir::new().unwrap();
    Command::new("git")
        .args(["clone", bare.to_str().unwrap(), "."])
        .current_dir(dir.path())
        .output()
        .expect("failed to clone");
    dir
}

fn stdout_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn local_branch_exists(work: &Path, branch: &str) -> bool {
    let output = git(
        work,
        &["branch", "--list", "--format=%(refname:short)", branch],
    );
    stdout_str(&output).lines().any(|name| name == branch)
}

fn remote_branch_tip(work: &Path, remote: &str, branch: &str) -> Option<String> {
    let refname = format!("refs/heads/{branch}");
    let output = git(work, &["ls-remote", "--heads", remote, &refname]);
    stdout_str(&output)
        .split_whitespace()
        .next()
        .map(String::from)
}

/// Just the names of the stale branches. Which ground each is stale on is
/// covered by the unit tests in `git`, against fixed refs rather than a repo.
fn stale_names(remote: &str) -> Vec<String> {
    perch::git::stale_branches(remote)
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn fast_forward_pull() {
    let (_bare, work) = setup();

    push_upstream_change(work.path(), "file.txt", "updated\n", "upstream change");

    let output = perch(work.path(), "main");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Pulled 1 commit"),
        "stderr: {}",
        stderr_str(&output)
    );

    let content = fs::read_to_string(work.path().join("file.txt")).unwrap();
    assert_eq!(content, "updated\n");
}

#[test]
fn auto_stash_and_restore() {
    let (_bare, work) = setup();

    // Track a second file so we can dirty it without conflicting with the pull.
    fs::write(work.path().join("other.txt"), "original\n").unwrap();
    git(work.path(), &["add", "other.txt"]);
    git(work.path(), &["commit", "-m", "add other"]);
    git(work.path(), &["push", "origin", "main"]);

    push_upstream_change(work.path(), "file.txt", "updated\n", "upstream change");

    // Dirty a tracked file.
    fs::write(work.path().join("other.txt"), "local work in progress\n").unwrap();

    let output = perch(work.path(), "main");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Pulled 1 commit"),
        "stderr: {}",
        stderr_str(&output)
    );

    // Local modification must survive the round-trip.
    let content = fs::read_to_string(work.path().join("other.txt")).unwrap();
    assert_eq!(content, "local work in progress\n");
}

#[test]
fn stash_pop_conflict_shows_guidance() {
    let (_bare, work) = setup();

    push_upstream_change(work.path(), "file.txt", "upstream version\n", "upstream");

    // Create a conflicting local modification to the same file.
    fs::write(work.path().join("file.txt"), "local version\n").unwrap();

    let output = perch(work.path(), "main");

    // The pull itself succeeds; only the stash pop conflicts.
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("Conflicts detected"),
        "expected conflict-specific guidance in stderr, got: {stderr}"
    );

    // The stash should still be present for manual recovery.
    let stash_list = git(work.path(), &["stash", "list"]);
    assert!(
        !stdout_str(&stash_list).is_empty(),
        "stash should not be empty after a failed pop"
    );
}

#[test]
fn diverged_branch_reports_error() {
    let (bare, work) = setup();

    // Create and push a feature branch.
    git(work.path(), &["checkout", "-b", "feature"]);
    fs::write(work.path().join("feature.txt"), "v1\n").unwrap();
    git(work.path(), &["add", "feature.txt"]);
    git(work.path(), &["commit", "-m", "feature v1"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    // Make a local-only commit so local is ahead.
    fs::write(work.path().join("feature.txt"), "local diverge\n").unwrap();
    git(work.path(), &["add", "feature.txt"]);
    git(work.path(), &["commit", "-m", "local diverge"]);

    // From a second clone, force-push a different commit to origin/feature.
    let second = clone_bare(bare.path());
    git(second.path(), &["checkout", "feature"]);
    fs::write(second.path().join("feature.txt"), "remote diverge\n").unwrap();
    git(second.path(), &["add", "feature.txt"]);
    git(second.path(), &["commit", "-m", "remote diverge"]);
    git(second.path(), &["push", "--force", "origin", "feature"]);

    let output = perch(work.path(), "feature");

    assert!(!output.status.success());

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("diverged"),
        "expected divergence message, got: {combined}"
    );
}

#[test]
fn refresh_dot_fast_forwards_clean_branch() {
    let (_bare, work) = setup();

    push_upstream_change(work.path(), "file.txt", "updated\n", "upstream change");

    let output = perch(work.path(), ".");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Pulled 1 commit"),
        "stderr: {}",
        stderr_str(&output)
    );
    let content = fs::read_to_string(work.path().join("file.txt")).unwrap();
    assert_eq!(content, "updated\n");
}

#[test]
fn refresh_dot_already_up_to_date_reports_so() {
    let (_bare, work) = setup();

    let output = perch(work.path(), ".");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Already up to date"),
        "stderr: {}",
        stderr_str(&output)
    );
}

#[test]
fn refresh_dot_clean_diverge_rebases_onto_remote() {
    let (bare, work) = setup();

    // A local commit on a different file than the one the remote advances, so
    // the branch diverges but the rebase replays cleanly.
    fs::write(work.path().join("other.txt"), "local work\n").unwrap();
    git(work.path(), &["add", "other.txt"]);
    git(work.path(), &["commit", "-m", "local work"]);

    let second = clone_bare(bare.path());
    push_upstream_change(
        second.path(),
        "file.txt",
        "remote change\n",
        "remote change",
    );
    git(work.path(), &["fetch", "origin"]);

    let output = perch(work.path(), ".");

    // Clean tree: the local commit is rebased on top of the remote commit with
    // no prompt. Both changes are present and origin/main is now in HEAD.
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(
        fs::read_to_string(work.path().join("other.txt")).unwrap(),
        "local work\n"
    );
    assert_eq!(
        fs::read_to_string(work.path().join("file.txt")).unwrap(),
        "remote change\n"
    );
    let behind = git(work.path(), &["rev-list", "--count", "HEAD..origin/main"]);
    assert_eq!(
        stdout_str(&behind).trim(),
        "0",
        "HEAD should contain origin/main after the rebase"
    );
}

#[test]
fn refresh_dot_clean_diverge_with_conflict_aborts() {
    let (bare, work) = setup();

    // Local commit and a rewritten origin commit that both touch file.txt, so
    // rebasing the local commit onto origin conflicts.
    fs::write(work.path().join("file.txt"), "local diverge\n").unwrap();
    git(work.path(), &["add", "file.txt"]);
    git(work.path(), &["commit", "-m", "local diverge"]);
    let local_head = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));

    let second = clone_bare(bare.path());
    push_upstream_change(
        second.path(),
        "file.txt",
        "remote rebase\n",
        "remote rebase",
    );
    git(work.path(), &["fetch", "origin"]);

    let output = perch(work.path(), ".");

    // The rebase conflicts, aborts, and restores the original HEAD.
    assert!(!output.status.success());
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("Rebase aborted"),
        "expected rebase-aborted message, got: {combined}"
    );
    let head_after = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));
    assert_eq!(
        head_after, local_head,
        "abort must restore the original HEAD"
    );
}

#[test]
fn refresh_dot_dirty_with_incoming_is_left_unchanged_non_interactively() {
    let (_bare, work) = setup();

    // Remote advances, then dirty a tracked file: there's work to integrate but
    // the tree is dirty, so a non-interactive run can't prompt and does nothing.
    push_upstream_change(work.path(), "file.txt", "remote change\n", "remote change");
    fs::write(work.path().join("file.txt"), "uncommitted edit\n").unwrap();
    let head_before = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));

    let output = perch(work.path(), ".");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("uncommitted changes") && stderr.contains("Left main unchanged"),
        "expected dirty-tree notice and no-op, got: {stderr}"
    );
    assert_eq!(
        stdout_str(&git(work.path(), &["rev-parse", "HEAD"])),
        head_before,
        "HEAD must not move without a prompt"
    );
    assert_eq!(
        fs::read_to_string(work.path().join("file.txt")).unwrap(),
        "uncommitted edit\n",
        "uncommitted changes must be preserved"
    );
}

#[test]
fn refresh_dot_with_unpushed_commit_reports_ahead_only() {
    let (_bare, work) = setup();

    // A local commit the remote doesn't have, but the remote hasn't moved on:
    // ahead, not diverged.
    fs::write(work.path().join("file.txt"), "local only\n").unwrap();
    git(work.path(), &["add", "file.txt"]);
    git(work.path(), &["commit", "-m", "local only"]);
    let local_head = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));

    let output = perch(work.path(), ".");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("1 commit ahead of origin/main") && stderr.contains("nothing to pull"),
        "expected ahead-only notice, got: {stderr}"
    );
    let head_after = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));
    assert_eq!(
        head_after, local_head,
        "HEAD must not move when nothing to pull"
    );
}

#[test]
fn refresh_dot_on_detached_head_errors() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "--detach"]);

    let output = perch(work.path(), ".");

    assert!(!output.status.success());
    assert!(
        stderr_str(&output).contains("not on a branch"),
        "stderr: {}",
        stderr_str(&output)
    );
}

#[test]
fn local_only_branch_not_stale_right_after_merge() {
    let (_bare, work) = setup();

    // Create a local-only branch (never pushed) and merge it into main.
    // Right after the merge HEAD == branch tip, so it's not stale yet.
    git(work.path(), &["checkout", "-b", "local-experiment"]);
    fs::write(work.path().join("experiment.txt"), "try something\n").unwrap();
    git(work.path(), &["add", "experiment.txt"]);
    git(work.path(), &["commit", "-m", "experiment"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "local-experiment"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"local-experiment".to_string()),
        "local-only branch should not be stale right after merge, got: {stale:?}"
    );
}

#[test]
fn local_only_branch_stale_after_main_advances() {
    let (_bare, work) = setup();

    // Create a local-only branch, merge it, then advance main past it.
    git(work.path(), &["checkout", "-b", "local-merged"]);
    fs::write(work.path().join("local.txt"), "work\n").unwrap();
    git(work.path(), &["add", "local.txt"]);
    git(work.path(), &["commit", "-m", "local work"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "local-merged"]);

    // Advance main past the branch.
    push_upstream_change(work.path(), "advance.txt", "new\n", "advance main");
    git(work.path(), &["pull", "origin", "main"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        stale.contains(&"local-merged".to_string()),
        "merged local branch behind HEAD should be stale, got: {stale:?}"
    );
}

/// A branch with no commits of its own can still match the untracked ref proxy.
/// Its row must report that evidence rather than claiming its dirty work landed.
#[test]
fn dirty_uncommitted_worktree_reports_its_ref_ground() {
    let (_bare, parent, work) = setup_with_parent();
    let held = parent.path().join("worktrees/repo/uncommitted");
    fs::create_dir_all(held.parent().unwrap()).unwrap();
    git(
        &work,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "uncommitted",
            held.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(held.join("file.txt"), "uncommitted work\n").unwrap();

    fs::write(work.join("advance.txt"), "advance main\n").unwrap();
    git(&work, &["add", "advance.txt"]);
    git(&work, &["commit", "-m", "advance main"]);
    git(&work, &["branch", "dest", "main"]);

    let text =
        console::strip_ansi_codes(&cleanup_prompt(&work, "dest", "uncommitted")).into_owned();

    assert!(
        text.contains("untracked, tip in anchor (+ worktree ●)"),
        "the row should state the ref evidence, got: {text}"
    );
}

/// Keeping a branch is about the sweep and nothing else: a branch `perch.keep`
/// names is stale by every rule and still never offered.
#[test]
fn a_kept_branch_is_never_offered_by_the_sweep() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "develop"]);
    fs::write(work.path().join("dev.txt"), "work\n").unwrap();
    git(work.path(), &["add", "dev.txt"]);
    git(work.path(), &["commit", "-m", "dev work"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "develop"]);
    push_upstream_change(work.path(), "advance.txt", "new\n", "advance main");
    git(work.path(), &["pull", "origin", "main"]);

    let _cwd = cwd_at(work.path());
    assert!(
        stale_names("origin").contains(&"develop".to_string()),
        "the branch must be stale before keeping it can mean anything",
    );

    git(work.path(), &["config", "--add", "perch.keep", "develop"]);

    let stale = stale_names("origin");
    assert!(
        !stale.contains(&"develop".to_string()),
        "a kept branch must not be offered, got: {stale:?}"
    );
}

/// A branch that published its own counterpart hands the question over to the
/// remote: once both are pushed, a branch whose commits main fast-forwarded
/// over is byte-identical to one pushed without any commits at all. Deleting
/// the remote branch is the signal that settles it.
#[test]
fn merged_tracked_branch_waits_for_its_upstream_to_go() {
    let (bare, work) = setup();

    git(work.path(), &["checkout", "-b", "feature-done"]);
    fs::write(work.path().join("feature.txt"), "done\n").unwrap();
    git(work.path(), &["add", "feature.txt"]);
    git(work.path(), &["commit", "-m", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature-done"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "feature-done"]);
    git(work.path(), &["push", "origin", "main"]);

    {
        let _cwd = cwd_at(work.path());
        let stale = stale_names("origin");
        assert!(
            !stale.contains(&"feature-done".to_string()),
            "a live upstream is indistinguishable from an unstarted branch, got: {stale:?}"
        );
    }

    git(bare.path(), &["branch", "-D", "feature-done"]);
    git(work.path(), &["fetch", "--prune", "origin"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");
    assert!(
        stale.contains(&"feature-done".to_string()),
        "a deleted upstream settles it, got: {stale:?}"
    );
}

/// The other half of the same ambiguity: a branch pushed before any work was
/// done on it. Judging it by its tip would offer it the moment you switched
/// away — and, being the branch just left, pre-tick it.
#[test]
fn empty_published_branch_is_not_stale_from_a_branch_past_main() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    // Somewhere further along than main, so the old ambient-HEAD rule and the
    // anchor rule disagree about `feature`.
    git(work.path(), &["checkout", "-b", "other"]);
    fs::write(work.path().join("other.txt"), "x\n").unwrap();
    git(work.path(), &["add", "other.txt"]);
    git(work.path(), &["commit", "-m", "other advances"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"feature".to_string()),
        "a branch pushed without commits must not be offered, got: {stale:?}"
    );
}

/// History is no more telling than the tip. An empty branch pushed at a merged
/// topic's tip presents the same refs as the topic itself, so no shape of
/// history can tell them apart.
#[test]
fn empty_published_branch_at_a_merged_tip_is_not_stale() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "topic"]);
    fs::write(work.path().join("topic.txt"), "work\n").unwrap();
    git(work.path(), &["add", "topic.txt"]);
    git(work.path(), &["commit", "-m", "topic work"]);
    git(work.path(), &["checkout", "main"]);
    git(
        work.path(),
        &["merge", "--no-ff", "-m", "merge topic", "topic"],
    );

    // Branch off the merged topic without adding anything, and publish it.
    git(work.path(), &["checkout", "-b", "feature", "topic"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    git(work.path(), &["checkout", "main"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"feature".to_string()),
        "a branch pushed without commits must not be offered, got: {stale:?}"
    );
    assert!(
        stale.contains(&"topic".to_string()),
        "the untracked topic that did the work should still be offered, got: {stale:?}"
    );
}

#[test]
fn tracked_branch_without_unique_commits_not_stale() {
    let (_bare, work) = setup();

    // Create and push a branch from main without adding any commits.
    git(work.path(), &["checkout", "-b", "new-feature"]);
    git(work.path(), &["push", "-u", "origin", "new-feature"]);
    git(work.path(), &["checkout", "main"]);

    // Simulate a pull that moves main ahead (branch is now behind HEAD).
    push_upstream_change(work.path(), "ahead.txt", "new\n", "advance main");
    git(work.path(), &["pull", "origin", "main"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"new-feature".to_string()),
        "branch with no unique commits should not be stale, got: {stale:?}"
    );
}

/// Adds a worktree the way `perch wt` does: a new branch off `origin/main`,
/// tracking it.
fn add_worktree_branch(work: &Path, parent: &Path, branch: &str) -> PathBuf {
    let path = parent.join("worktrees").join("repo").join(branch);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        work,
        &[
            "worktree",
            "add",
            "--track",
            "-b",
            branch,
            path.to_str().unwrap(),
            "origin/main",
        ],
    );
    path
}

/// Adds a worktree with no branch of its own, the state a worktree reports once
/// its directory has been deleted by hand — and the one case where its directory
/// name is the only handle `wt rm` has on it.
fn add_worktree_detached(work: &Path, parent: &Path, name: &str) -> PathBuf {
    let path = parent.join("worktrees").join("repo").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        work,
        &["worktree", "add", "--detach", path.to_str().unwrap()],
    );
    path
}

/// A branch created off `origin/main` and never committed to has its tip *equal*
/// to main's, which the old `tip == HEAD` rule read as "fast-forward merged".
#[test]
fn fresh_worktree_branch_is_not_stale_from_the_main_worktree() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree_branch(&work, parent.path(), "feature-a");

    let _cwd = cwd_at(&work);
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"feature-a".to_string()),
        "an empty worktree branch should not be stale, got: {stale:?}"
    );
}

/// The reported bug: staleness used to be judged against ambient HEAD, so
/// committing in one worktree made every *sibling* worktree's branch look
/// merged-and-behind.
#[test]
fn fresh_worktree_branch_is_not_stale_from_a_sibling_worktree() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree_branch(&work, parent.path(), "feature-a");
    let b = add_worktree_branch(&work, parent.path(), "feature-b");

    fs::write(b.join("work.txt"), "work\n").unwrap();
    git(&b, &["add", "work.txt"]);
    git(&b, &["commit", "-m", "sibling work"]);

    let _cwd = cwd_at(&b);
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"feature-a".to_string()),
        "a sibling's commits must not make feature-a stale, got: {stale:?}"
    );
}

/// `perch wt`'s own merge-locally workflow: a worktree branch that did real
/// work, fast-forwarded into main. It tracks `origin/main` rather than a
/// counterpart of its own, so only being *ahead* of what it tracks separates it
/// from a branch that never held a commit.
#[test]
fn worktree_branch_fast_forwarded_into_main_is_stale() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree_branch(&work, parent.path(), "feature-a");

    fs::write(path.join("work.txt"), "real work\n").unwrap();
    git(&path, &["add", "work.txt"]);
    git(&path, &["commit", "-m", "real work"]);
    git(&work, &["merge", "--ff-only", "feature-a"]);

    let _cwd = cwd_at(&work);
    let stale = stale_names("origin");

    assert!(
        stale.contains(&"feature-a".to_string()),
        "a worktree branch merged into main should be stale, got: {stale:?}"
    );
}

/// How a merge commit reshapes history is not evidence either way, so a `wt`
/// branch merged with `--no-ff` rests on the same ahead count as any other:
/// offered while main still holds commits its upstream doesn't, and silent once
/// main is pushed and the count falls back to zero.
#[test]
fn no_ff_merged_branch_is_stale_until_main_is_pushed() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree_branch(&work, parent.path(), "feature-noff");

    fs::write(path.join("noff.txt"), "work\n").unwrap();
    git(&path, &["add", "noff.txt"]);
    git(&path, &["commit", "-m", "work"]);
    git(
        &work,
        &["merge", "--no-ff", "-m", "merge feature", "feature-noff"],
    );

    {
        let _cwd = cwd_at(&work);
        let stale = stale_names("origin");
        assert!(
            stale.contains(&"feature-noff".to_string()),
            "work main holds and origin/main doesn't should be offered, got: {stale:?}"
        );
    }

    git(&work, &["push", "origin", "main"]);

    let _cwd = cwd_at(&work);
    let stale = stale_names("origin");
    assert!(
        !stale.contains(&"feature-noff".to_string()),
        "once pushed it cannot be told from an untouched branch, got: {stale:?}"
    );
}

/// The same shape reached from the other side: an empty branch pointed at a
/// merged topic's tip and set to track `origin/main`. Nothing separates it from
/// the merged worktree branch above once main is pushed, so neither is offered.
#[test]
fn empty_anchor_tracking_branch_at_a_merged_tip_is_not_stale() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "topic"]);
    fs::write(work.path().join("topic.txt"), "work\n").unwrap();
    git(work.path(), &["add", "topic.txt"]);
    git(work.path(), &["commit", "-m", "topic work"]);
    git(work.path(), &["checkout", "main"]);
    git(
        work.path(),
        &["merge", "--no-ff", "-m", "merge topic", "topic"],
    );
    git(work.path(), &["push", "origin", "main"]);

    git(work.path(), &["branch", "feature", "topic"]);
    git(
        work.path(),
        &["branch", "--set-upstream-to=origin/main", "feature"],
    );

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"feature".to_string()),
        "a branch with no commits of its own must not be offered, got: {stale:?}"
    );
    assert!(
        stale.contains(&"topic".to_string()),
        "the untracked topic that did the work should still be offered, got: {stale:?}"
    );
}

/// Without a default branch there is nothing to judge "merged" against, so
/// the merged rule stands down rather than falling back to ambient HEAD. A
/// deleted upstream still speaks for itself.
#[test]
fn without_a_default_branch_only_gone_upstreams_are_stale() {
    let (bare, work) = setup();

    // A merged branch that would otherwise qualify.
    git(work.path(), &["checkout", "-b", "merged-work"]);
    fs::write(work.path().join("merged.txt"), "work\n").unwrap();
    git(work.path(), &["add", "merged.txt"]);
    git(work.path(), &["commit", "-m", "work"]);
    git(work.path(), &["checkout", "main"]);
    git(
        work.path(),
        &["merge", "--no-ff", "-m", "merge", "merged-work"],
    );

    // A branch whose upstream is deleted on the remote.
    git(work.path(), &["checkout", "-b", "abandoned"]);
    git(work.path(), &["push", "-u", "origin", "abandoned"]);
    git(work.path(), &["checkout", "main"]);
    git(bare.path(), &["branch", "-D", "abandoned"]);
    git(work.path(), &["fetch", "--prune", "origin"]);

    git(work.path(), &["remote", "set-head", "origin", "--delete"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("origin");

    assert!(
        !stale.contains(&"merged-work".to_string()),
        "no anchor means no merged rule, got: {stale:?}"
    );
    assert!(
        stale.contains(&"abandoned".to_string()),
        "a gone upstream is stale with or without an anchor, got: {stale:?}"
    );
}

/// The upstream a new worktree branch carries is load-bearing for the staleness
/// rules, so it must not depend on the user's `branch.autoSetupMerge`.
#[test]
fn worktree_add_sets_upstream_with_auto_setup_merge_off() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["config", "branch.autoSetupMerge", "false"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    let _cwd = cwd_at(&work);
    perch::git::worktree_add(&path, "feature", Some("origin/main")).unwrap();

    let upstream = git(
        &work,
        &["for-each-ref", "--format=%(upstream)", "refs/heads/feature"],
    );
    assert_eq!(
        stdout_str(&upstream).trim(),
        "refs/remotes/origin/main",
        "worktree branches must track what they were created from, whatever the config"
    );
}

#[test]
fn force_delete_branch_removes_branch() {
    let (_bare, work) = setup();

    for name in ["feat-a", "feat-b"] {
        git(work.path(), &["checkout", "-b", name]);
        fs::write(work.path().join(format!("{name}.txt")), "x\n").unwrap();
        git(work.path(), &["add", "."]);
        git(work.path(), &["commit", "-m", name]);
        git(work.path(), &["checkout", "main"]);
        git(
            work.path(),
            &["merge", "--no-ff", name, "-m", &format!("merge {name}")],
        );
    }

    let _cwd = cwd_at(work.path());
    for name in ["feat-a", "feat-b"] {
        let outcome = perch::git::force_delete_branch(None, name)
            .expect("force_delete_branch should not error");
        assert!(
            matches!(outcome, perch::git::BranchDeleteOutcome::Deleted),
            "{name} should report as deleted"
        );
    }

    let listing = git(work.path(), &["branch", "--format=%(refname:short)"]);
    let names = stdout_str(&listing);
    for name in ["feat-a", "feat-b"] {
        assert!(
            !names.lines().any(|l| l == name),
            "{name} should be deleted, got: {names}"
        );
    }
}

#[test]
fn worktree_held_stale_branch_is_no_longer_reported_as_skipped() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "wip"]);
    fs::write(work.path().join("wip.txt"), "x\n").unwrap();
    git(work.path(), &["add", "wip.txt"]);
    git(work.path(), &["commit", "-m", "wip"]);
    git(work.path(), &["push", "-u", "origin", "wip"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "wip"]);
    git(work.path(), &["push", "origin", "main"]);

    let parent = TempDir::new().unwrap();
    let worktree_path = parent.path().join("wt");
    git(
        work.path(),
        &["worktree", "add", worktree_path.to_str().unwrap(), "wip"],
    );

    let output = perch(work.path(), "main");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    // A held stale branch is now offered in the prompt alongside its worktree
    // rather than dismissed as unactionable.
    let stderr = stderr_str(&output);
    assert!(
        !stderr.contains("skipping"),
        "the dead-end skip message should be gone, got: {stderr}"
    );

    // Non-interactively there's no prompt, so nothing is destroyed: the branch
    // and its worktree both survive.
    let branches = git(
        work.path(),
        &["branch", "--list", "--format=%(refname:short)"],
    );
    assert!(
        stdout_str(&branches).lines().any(|l| l == "wip"),
        "wip should still exist, got: {}",
        stdout_str(&branches)
    );
    assert!(
        worktree_path.is_dir(),
        "worktree should survive a non-interactive run: {}",
        worktree_path.display()
    );
}

/// The candidate list the shell completions ask for. The main worktree is never
/// removable so it is never offered; every other one is offered under both names
/// `wt rm` accepts, which is what the awk this replaced could not do. `--complete`
/// is also read before anything destructive, so the worktrees are all still there
/// afterwards even though a bare `wt rm` here would offer to remove them.
#[test]
fn wt_rm_complete_lists_every_name_rm_accepts() {
    let (_bare, parent, work) = setup_with_parent();
    let live = add_worktree_branch(&work, parent.path(), "feat/login");

    // Detached: no branch, so its directory name is the only handle on it.
    add_worktree_detached(&work, parent.path(), "spike");

    // Prunable: registered but gone from disk, which is what `wt rm` is for.
    let gone = add_worktree_branch(&work, parent.path(), "abandoned");
    fs::remove_dir_all(&gone).unwrap();

    let output = perch_args(&work, &["wt", "rm", "--complete"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let stdout = stdout_str(&output);
    let mut names: Vec<&str> = stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, ["abandoned", "feat/login", "login", "spike"]);

    assert!(live.is_dir(), "listing must not remove: {}", live.display());
}

/// The branches offered where a branch name goes, one position at a time. Each
/// position subtracts what its own dispatcher arm eats and nothing more, so a
/// word eaten after `wt` is still offered at the top level and a *Verb* is still
/// offered after `br` — collision is positional, and completing a name the
/// dispatcher there would swallow is what this guards against.
#[test]
fn complete_drops_only_the_words_that_position_eats() {
    let (_bare, work) = setup();
    for branch in ["br", "wt", "ls", "rm", "list", "remove", "feat/x"] {
        git(work.path(), &["branch", branch]);
    }

    let offered = |args: &[&str]| {
        let output = perch_args(work.path(), args);
        assert!(output.status.success(), "stderr: {}", stderr_str(&output));
        let mut names: Vec<String> = stdout_str(&output).lines().map(String::from).collect();
        names.sort();
        names
    };

    assert_eq!(
        offered(&["--complete"]),
        ["feat/x", "list", "ls", "main", "remove", "rm"],
        "a bare `perch` reads `br` and `wt` as verbs"
    );
    assert_eq!(
        offered(&["br", "--complete"]),
        ["br", "feat/x", "list", "ls", "main", "remove", "wt"],
        "`br` reads `rm` as its subverb"
    );
    assert_eq!(
        offered(&["br", "rm", "--complete"]),
        ["br", "feat/x", "list", "ls", "main", "remove", "rm", "wt"],
        "`br rm` accepts every local branch, including one named `rm`"
    );
    assert_eq!(
        offered(&["wt", "--complete"]),
        ["br", "feat/x", "main", "wt"],
        "`wt` reads the two subverbs and the two retired spellings"
    );
    assert_eq!(
        offered(&["wt", "--no-switch", "--complete"]),
        ["br", "feat/x", "main", "wt"],
        "a `wt` option leaves the branch position unchanged"
    );
    assert_eq!(
        offered(&["wt", "short", "--complete"]),
        ["br", "feat/x", "list", "ls", "main", "remove", "rm", "wt"],
        "the second `wt` positional is a branch and eats no subverb spellings"
    );

    // `--` is what you type to reach a name some position would eat, so it has
    // to answer with every branch — at whichever level it was typed. Git
    // refuses a branch whose name begins with `-`, so reading `--complete`
    // here costs no name that was ever reachable.
    let everything = ["br", "feat/x", "list", "ls", "main", "remove", "rm", "wt"];
    assert_eq!(offered(&["--", "--complete"]), everything);
    assert_eq!(offered(&["br", "--", "--complete"]), everything);
    assert_eq!(offered(&["wt", "--", "--complete"]), everything);
    assert_eq!(offered(&["wt", "--", "short", "--complete"]), everything);
    assert_eq!(
        offered(&["wt", "--no-switch", "--", "--complete"]),
        everything
    );
}

#[test]
fn bash_br_rm_completion_offers_one_target_and_only_long_flags() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.bash");
    let complete = |words: &str, cword: usize| {
        let script = format!(
            "PATH=\"{bin}\":$PATH\n\
             source \"{completions}\"\n\
             COMP_WORDS=({words}); COMP_CWORD={cword}; COMPREPLY=()\n\
             _perch_completions\n\
             printf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
            bin = bin.display(),
        );
        let output = Command::new("bash")
            .args(["-c", &script])
            .current_dir(work.path())
            .env("PERCH_NO_HOOKS", "1")
            .output()
            .expect("failed to run bash completion");
        assert!(output.status.success(), "stderr: {}", stderr_str(&output));
        stdout_str(&output)
    };

    let before_target = complete("perch br rm ''", 3);
    for candidate in ["feature", "--upstream", "--force"] {
        assert!(
            before_target.lines().any(|line| line == candidate),
            "missing {candidate}: {before_target}"
        );
    }
    assert!(!before_target.lines().any(|line| line == "-f"));

    let after_target = complete("perch br rm feature ''", 4);
    assert!(!after_target.lines().any(|line| line == "feature"));
    assert!(after_target.lines().any(|line| line == "--upstream"));
    assert!(after_target.lines().any(|line| line == "--force"));

    let after_rejected_escape = complete("perch br rm -- ''", 4);
    assert!(
        !after_rejected_escape.lines().any(|line| line == "feature"),
        "br rm rejects `--`, so completion must not offer a target after it: {after_rejected_escape}"
    );
}

#[test]
fn zsh_br_rm_completion_rejects_a_double_dash() {
    if Command::new("zsh").arg("--version").output().is_err() {
        return;
    }
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/_perch");
    let script = format!(
        "function _describe {{ : }}\n\
         service=skip\n\
         source \"{completions}\"\n\
         function _perch_offers {{ print -r -- \"$*\" }}\n\
         service=perch\n\
         words=(perch br rm -- '')\n\
         CURRENT=5\n\
         _perch\n"
    );

    let output = Command::new("zsh")
        .args(["-c", &script])
        .output()
        .expect("failed to run zsh completion");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(stdout_str(&output), "");
}

#[test]
fn fish_br_rm_completion_rejects_a_double_dash() {
    if Command::new("fish").arg("--version").output().is_err() {
        return;
    }
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.fish");
    let script = format!("source \"{completions}\"\ncomplete -C 'perch br rm -- '\n");

    let output = Command::new("fish")
        .args(["-c", &script])
        .current_dir(work.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run fish completion");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let completions = stdout_str(&output);
    assert!(
        !completions.lines().any(|line| line.starts_with("feature")),
        "br rm rejects `--`, so fish must not offer a target after it: {completions}"
    );
}

#[test]
fn bash_wt_completion_offers_branches_after_a_worktree_name() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.bash");
    let script = format!(
        "PATH=\"{bin}\":$PATH\n\
         source \"{completions}\"\n\
         COMP_WORDS=(perch wt short ''); COMP_CWORD=3; COMPREPLY=()\n\
         _perch_completions\n\
         printf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
        bin = bin.display(),
    );

    let output = Command::new("bash")
        .args(["-c", &script])
        .current_dir(work.path())
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run bash completion");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(stdout_str(&output).lines().any(|line| line == "feature"));
}

#[test]
fn zsh_wt_completion_asks_for_branches_after_a_worktree_name() {
    if Command::new("zsh").arg("--version").output().is_err() {
        return;
    }
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/_perch");
    let script = format!(
        "function _describe {{ : }}\n\
         service=skip\n\
         source \"{completions}\"\n\
         function _perch_offers {{ print -r -- \"$*\" }}\n\
         service=perch\n\
         words=(perch wt short '')\n\
         CURRENT=4\n\
         _perch\n"
    );

    let output = Command::new("zsh")
        .args(["-c", &script])
        .output()
        .expect("failed to run zsh completion");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stdout_str(&output)
            .lines()
            .any(|line| line == "branches branch wt short")
    );
}

#[test]
fn fish_wt_completion_offers_branches_after_a_worktree_name() {
    if Command::new("fish").arg("--version").output().is_err() {
        return;
    }
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    let completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.fish");
    let script = format!("source \"{completions}\"\ncomplete -C 'perch wt short '\n");

    let output = Command::new("fish")
        .args(["-c", &script])
        .current_dir(work.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run fish completion");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stdout_str(&output)
            .lines()
            .any(|line| line.starts_with("feature"))
    );
}

/// Regression, in both halves. Git permits `$`, backticks and `${IFS}` in a ref
/// name, so a branch can be called `$(…)` — and once remote-only branches were
/// offered, that name came from whoever can push to a repo you fetch rather than
/// from you.
///
/// Offering it must not run it: `compgen -W` expanded its word list before
/// matching, so the payload fired on TAB. Nor must inserting it: bash puts a
/// match on the command line verbatim, so an unescaped candidate fires on the
/// Enter that follows. The payload would write a file, so its absence covers the
/// first; the candidate coming back escaped covers the second.
///
/// Drives the completion the way bash does: source the file, set the words, call
/// the function.
#[test]
fn a_branch_named_like_a_command_substitution_does_not_run_on_tab() {
    let (_bare, work) = setup();
    let payload = work.path().join("pwned");
    // Relative, and no space: a ref name may hold neither a space nor a path
    // component starting with `.`, which a temp directory's does. `${IFS}`
    // supplies the space, and bash runs below with the repo as its cwd.
    git(work.path(), &["branch", "$(touch${IFS}pwned)"]);

    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    // Both paths quoted: a checkout directory may hold a space, and an
    // unsourced completion file would leave COMPREPLY empty and the assertions
    // below passing for the wrong reason.
    let script = format!(
        "PATH=\"{bin}\":$PATH\n\
         source \"{completions}\"\n\
         COMP_WORDS=(perch ''); COMP_CWORD=1; COMPREPLY=()\n\
         _perch_completions\n\
         printf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
        bin = bin.display(),
        completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.bash"),
    );
    let output = Command::new("bash")
        .args(["-c", &script])
        .current_dir(work.path())
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run bash");

    let stdout = stdout_str(&output);
    assert!(
        !payload.exists(),
        "offering a branch must not execute its name; stdout: {stdout}"
    );
    // The completion still has to work, or every assertion here is vacuous.
    assert!(
        stdout.lines().any(|l| l == "main"),
        "expected `main` among the candidates, got: {stdout}"
    );
    // Offered, but as text a shell reads literally — the raw spelling on the
    // command line would run on Enter.
    let offered = stdout
        .lines()
        .find(|l| l.contains("touch"))
        .unwrap_or_else(|| panic!("the branch should still be offered, got: {stdout}"));
    assert!(
        offered.contains('\\'),
        "the candidate should be escaped for insertion, got: {offered}"
    );
}

/// Escaping candidates for insertion costs the word on the command line its
/// identity with the name. Where several share a prefix, bash inserts that
/// prefix and the next TAB arrives with `$cur` in escaped spelling — `feat\&`
/// for `feat&one` and `feat&two` — which no raw name starts with. Matching only
/// the raw spelling would answer nothing there, dead-ending completion at the
/// point it should be narrowing. Drives the second TAB: the word is what the
/// first one left behind.
#[test]
fn a_second_tab_still_narrows_after_the_first_inserted_an_escaped_prefix() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feat&one"]);
    git(work.path(), &["branch", "feat&two"]);

    let bin = Path::new(env!("CARGO_BIN_EXE_perch")).parent().unwrap();
    let script = format!(
        "PATH=\"{bin}\":$PATH\n\
         source \"{completions}\"\n\
         COMP_WORDS=(perch 'feat\\&'); COMP_CWORD=1; COMPREPLY=()\n\
         _perch_completions\n\
         printf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
        bin = bin.display(),
        completions = concat!(env!("CARGO_MANIFEST_DIR"), "/completions/perch.bash"),
    );
    let output = Command::new("bash")
        .args(["-c", &script])
        .current_dir(work.path())
        .env("PERCH_NO_HOOKS", "1")
        .output()
        .expect("failed to run bash");

    let stdout = stdout_str(&output);
    let mut names: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [r"feat\&one", r"feat\&two"],
        "both branches should still be offered, and still escaped; got: {stdout}"
    );
}

/// The gap this replaced `git branch` to close: a branch that exists only on the
/// remote is in the picker and is accepted as a named target, so it has to be
/// offered too. `git branch` cannot see one.
#[test]
fn complete_offers_a_branch_that_exists_only_on_the_remote() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "published"]);
    git(work.path(), &["push", "origin", "published"]);
    git(work.path(), &["branch", "-D", "published"]);

    let output = perch_args(work.path(), &["--complete"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let stdout = stdout_str(&output);
    let mut names: Vec<&str> = stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, ["main", "published"]);
}

#[test]
fn help_flag_prints_usage() {
    let dir = TempDir::new().unwrap();
    let output = perch(dir.path(), "--help");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let out = stdout_str(&output);
    assert!(
        out.contains("Usage: perch"),
        "expected usage line, got: {out}"
    );
    assert!(
        out.contains("perch wt"),
        "expected worktree usage in help, got: {out}"
    );
    // The footer is the only place the shell shortcuts are advertised, so it is
    // how anyone learns `br`/`wt` exist and that they can be turned off.
    assert!(
        out.contains("PERCH_NO_SHORTCUTS"),
        "expected the shell shortcut footer in help, got: {out}"
    );
}

#[test]
fn help_pages_remain_exact_static_text() {
    let dir = TempDir::new().unwrap();
    for (args, expected) in [
        (&["--help"][..], include_str!("fixtures/help/main.txt")),
        (
            &["br", "--help"][..],
            include_str!("fixtures/help/branch.txt"),
        ),
        (
            &["wt", "--help"][..],
            include_str!("fixtures/help/worktree.txt"),
        ),
    ] {
        let output = perch_args(dir.path(), args);
        assert!(output.status.success(), "stderr: {}", stderr_str(&output));
        assert_eq!(stdout_str(&output), expected);
    }
}

#[test]
fn wt_help_documents_no_switch() {
    let dir = TempDir::new().unwrap();
    let output = perch_args(dir.path(), &["wt", "--help"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stdout_str(&output).contains("--no-switch"),
        "expected worktree help to document the flag, got: {}",
        stdout_str(&output)
    );
}

#[test]
fn br_help_documents_removal_without_a_short_force_flag() {
    let dir = TempDir::new().unwrap();
    let output = perch_args(dir.path(), &["br", "--help"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let help = stdout_str(&output);
    assert!(help.contains("perch br rm [<branch>] [--upstream] [--force]"));
    assert!(help.contains("perch br -- <branch>"));
    assert!(!help.contains("-f, --force"));
}

#[test]
fn version_flag_prints_version() {
    let dir = TempDir::new().unwrap();
    let output = perch(dir.path(), "--version");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(
        stdout_str(&output).trim(),
        format!("perch {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn non_origin_remote_pulls_via_branch_config() {
    let (_bare, work) = setup_with_remote("upstream");

    push_upstream_change_to(
        work.path(),
        "upstream",
        "file.txt",
        "updated\n",
        "upstream change",
    );

    let output = perch(work.path(), "main");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Pulled 1 commit"),
        "stderr: {}",
        stderr_str(&output)
    );

    let content = fs::read_to_string(work.path().join("file.txt")).unwrap();
    assert_eq!(content, "updated\n");
}

#[test]
fn non_origin_remote_detects_stale_branch() {
    let (bare, work) = setup_with_remote("upstream");

    git(work.path(), &["checkout", "-b", "feature-done"]);
    fs::write(work.path().join("feature.txt"), "done\n").unwrap();
    git(work.path(), &["add", "feature.txt"]);
    git(work.path(), &["commit", "-m", "feature"]);
    git(work.path(), &["push", "-u", "upstream", "feature-done"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["merge", "feature-done"]);
    git(work.path(), &["push", "upstream", "main"]);
    git(bare.path(), &["branch", "-D", "feature-done"]);
    git(work.path(), &["fetch", "--prune", "upstream"]);

    let _cwd = cwd_at(work.path());
    let stale = stale_names("upstream");

    assert!(
        stale.contains(&"feature-done".to_string()),
        "merged branch with upstream remote should be stale, got: {stale:?}"
    );
}

#[test]
fn current_remote_handles_multiline_config_value() {
    let (_bare, work) = setup_with_remote("upstream");

    git(
        work.path(),
        &["config", "branch.main.remote", "upstream\nstray"],
    );

    let _cwd = cwd_at(work.path());
    let remote = perch::git::current_remote(Some("main"));

    assert_eq!(remote, "upstream");
}

#[test]
fn rebase_replays_local_commits_onto_remote() {
    let (bare, work) = setup();

    git(work.path(), &["checkout", "-b", "feature"]);
    fs::write(work.path().join("feature.txt"), "base\n").unwrap();
    git(work.path(), &["add", "feature.txt"]);
    git(work.path(), &["commit", "-m", "feature base"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    // Local-only commit on a unique file (no conflict).
    fs::write(work.path().join("local.txt"), "local\n").unwrap();
    git(work.path(), &["add", "local.txt"]);
    git(work.path(), &["commit", "-m", "local commit"]);

    // From a second clone, push a different commit on a different file.
    let second = clone_bare(bare.path());
    git(second.path(), &["checkout", "feature"]);
    fs::write(second.path().join("remote.txt"), "remote\n").unwrap();
    git(second.path(), &["add", "remote.txt"]);
    git(second.path(), &["commit", "-m", "remote commit"]);
    git(second.path(), &["push", "origin", "feature"]);

    git(work.path(), &["fetch", "origin"]);

    let _cwd = cwd_at(work.path());
    let outcome = perch::git::rebase("origin/feature").expect("rebase call failed");

    assert!(
        matches!(outcome, perch::git::RebaseOutcome::Clean),
        "expected Clean rebase outcome"
    );
    assert!(
        work.path().join("local.txt").exists(),
        "local.txt should survive the rebase"
    );
    assert!(
        work.path().join("remote.txt").exists(),
        "remote.txt should be present after rebase"
    );
}

#[test]
fn rebase_aborts_on_conflict_and_leaves_clean_tree() {
    let (bare, work) = setup();

    git(work.path(), &["checkout", "-b", "feature"]);
    fs::write(work.path().join("file.txt"), "base\n").unwrap();
    git(work.path(), &["add", "file.txt"]);
    git(work.path(), &["commit", "-m", "feature base"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    // Conflicting local change.
    fs::write(work.path().join("file.txt"), "local\n").unwrap();
    git(work.path(), &["add", "file.txt"]);
    git(work.path(), &["commit", "-m", "local"]);

    // Conflicting remote change (force-pushed from a second clone).
    let second = clone_bare(bare.path());
    git(second.path(), &["checkout", "feature"]);
    fs::write(second.path().join("file.txt"), "remote\n").unwrap();
    git(second.path(), &["add", "file.txt"]);
    git(second.path(), &["commit", "-m", "remote"]);
    git(second.path(), &["push", "--force", "origin", "feature"]);

    git(work.path(), &["fetch", "origin"]);

    let _cwd = cwd_at(work.path());
    let outcome = perch::git::rebase("origin/feature").expect("rebase call failed");

    assert!(
        matches!(outcome, perch::git::RebaseOutcome::Aborted),
        "expected Aborted rebase outcome"
    );

    let git_dir = work.path().join(".git");
    assert!(
        !git_dir.join("rebase-merge").exists(),
        "rebase-merge directory should not exist after abort"
    );
    assert!(
        !git_dir.join("rebase-apply").exists(),
        "rebase-apply directory should not exist after abort"
    );
}

#[test]
fn detached_head_can_switch_to_branch() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "--detach", "HEAD"]);

    let output = perch(work.path(), "main");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let head = git(work.path(), &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "main");
}

#[test]
fn wt_creates_worktree_for_existing_local_branch() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let expected = parent.path().join("worktrees").join("repo").join("feature");
    assert!(
        expected.exists(),
        "worktree should exist at {}",
        expected.display()
    );
    assert!(
        stdout_str(&output).trim().ends_with("repo/feature"),
        "stdout should be the worktree path; got: {}",
        stdout_str(&output)
    );

    let list = git(&work, &["worktree", "list", "--porcelain"]);
    let s = stdout_str(&list);
    assert!(
        s.contains("branch refs/heads/feature"),
        "expected `feature` worktree; got: {s}"
    );
}

#[test]
fn wt_uses_an_explicit_directory_name_for_a_local_branch() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "renovate/realm-swiftlint-0.x"]);

    let output = perch_args(&work, &["wt", "545", "renovate/realm-swiftlint-0.x"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let expected = parent.path().join("worktrees").join("repo").join("545");
    assert!(
        expected.is_dir(),
        "missing worktree: {}",
        expected.display()
    );
    let branch = git(&expected, &["branch", "--show-current"]);
    assert_eq!(stdout_str(&branch).trim(), "renovate/realm-swiftlint-0.x");
    assert_eq!(
        Path::new(stdout_str(&output).trim())
            .canonicalize()
            .unwrap(),
        expected.canonicalize().unwrap()
    );
}

#[test]
fn wt_uses_an_explicit_directory_name_for_a_remote_only_branch() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "published"]);
    git(&work, &["push", "origin", "published"]);
    git(&work, &["branch", "-D", "published"]);

    let output = perch_args(&work, &["wt", "short", "published"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let expected = parent.path().join("worktrees").join("repo").join("short");
    let branch = git(&expected, &["branch", "--show-current"]);
    assert_eq!(stdout_str(&branch).trim(), "published");
}

#[test]
fn wt_uses_an_explicit_directory_name_for_a_new_branch() {
    let (_bare, parent, work) = setup_with_parent();

    let output = perch_args(&work, &["wt", "short", "brand-new"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let expected = parent.path().join("worktrees").join("repo").join("short");
    let branch = git(&expected, &["branch", "--show-current"]);
    assert_eq!(stdout_str(&branch).trim(), "brand-new");
}

#[test]
fn wt_uses_the_registered_worktree_instead_of_an_explicit_directory_name() {
    let (_bare, parent, work) = setup_with_parent();
    let existing = add_worktree(&work, &parent, "feature");

    let output = perch_args(&work, &["wt", "ignored", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(
        Path::new(stdout_str(&output).trim())
            .canonicalize()
            .unwrap(),
        existing.canonicalize().unwrap()
    );
    assert!(!parent.path().join("worktrees/repo/ignored").exists());
}

#[test]
fn wt_explicit_directory_name_supports_no_switch() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "short", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(stdout_str(&output), "");
    assert!(parent.path().join("worktrees/repo/short").is_dir());
}

#[test]
fn wt_escape_allows_a_directory_name_that_matches_a_subverb() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "--", "rm", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(parent.path().join("worktrees/repo/rm").is_dir());
}

#[test]
fn wt_escape_allows_an_option_looking_directory_name() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "--", "--noswitch", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(parent.path().join("worktrees/repo/--noswitch").is_dir());
}

#[test]
fn wt_rejects_an_unknown_option_before_the_directory_name() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "--noswitch", "feature"]);

    assert!(!output.status.success());
    assert!(stderr_str(&output).contains("unknown option '--noswitch'"));
    assert!(!parent.path().join("worktrees/repo/--noswitch").exists());
}

#[test]
fn wt_rejects_an_unknown_option_in_the_branch_position() {
    let (_bare, parent, work) = setup_with_parent();

    let output = perch_args(&work, &["wt", "short", "--typo"]);

    assert!(!output.status.success());
    assert!(stderr_str(&output).contains("unknown option '--typo'"));
    assert!(!parent.path().join("worktrees/repo/short").exists());
}

#[test]
fn wt_rejects_invalid_directory_names_and_a_third_argument() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    for name in [".", "..", "nested/name", r"nested\name"] {
        let output = perch_args(&work, &["wt", name, "feature"]);
        assert!(!output.status.success(), "{name:?} should be rejected");
        assert!(stderr_str(&output).contains("invalid worktree directory name"));
    }

    let output = perch_args(&work, &["wt", "short", "feature", "extra"]);
    assert!(!output.status.success());
    assert!(stderr_str(&output).contains("unexpected extra argument 'extra'"));
    assert!(!parent.path().join("worktrees/repo/short").exists());
}

#[test]
fn wt_creation_does_not_write_cursor_controls_without_a_terminal() {
    let (_bare, _parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let output = perch_args(&work, &["wt", "feature"]);

    assert!(
        !output.stderr.windows(5).any(|bytes| bytes == b"\x1b[?25"),
        "stderr should not contain cursor controls: {:?}",
        output.stderr
    );
}

#[test]
fn wt_creates_new_branch_from_default_when_branch_absent() {
    let (_bare, parent, work) = setup_with_parent();

    let output = perch_args(&work, &["wt", "brand-new"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let expected = parent
        .path()
        .join("worktrees")
        .join("repo")
        .join("brand-new");
    assert!(
        expected.exists(),
        "worktree should exist at {}",
        expected.display()
    );

    // The new branch should have a base commit (from origin/main).
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&expected)
        .output()
        .unwrap();
    assert!(head.status.success(), "stderr: {}", stderr_str(&head));
}

#[test]
fn wt_no_switch_creates_a_new_branch_without_a_shell_handoff() {
    let (_bare, parent, work) = setup_with_parent();

    let output = perch_args(&work, &["wt", "brand-new", "--no-switch"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let expected = parent
        .path()
        .join("worktrees")
        .join("repo")
        .join("brand-new");
    assert!(
        expected.is_dir(),
        "worktree should exist at {}",
        expected.display()
    );
    assert_eq!(
        stdout_str(&output),
        "",
        "--no-switch must not print a path for the shell wrapper"
    );
}

#[test]
fn wt_no_switch_finds_an_existing_worktree_without_claiming_to_switch() {
    let (_bare, parent, work) = setup_with_parent();

    add_worktree(&work, &parent, "feature");

    let output = perch_args(&work, &["wt", "--no-switch", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(
        stdout_str(&output),
        "",
        "--no-switch must not print a path for the shell wrapper"
    );
    assert!(
        !stderr_str(&output).contains("switched to"),
        "the status must not claim a switch happened, got: {}",
        stderr_str(&output)
    );
}

/// The `git fetch` invocations a `perch` run made, read back from a `GIT_TRACE`
/// file every git it spawned appends to — the background fetch's own output
/// goes nowhere, so tracing to the terminal would miss it.
fn perch_traced(parent: &TempDir, work: &Path, args: &[&str]) -> (Output, Vec<String>) {
    perch_traced_with(parent, perch_command(work, args))
}

/// [`perch_traced`], for a `perch` command the caller has already set up.
fn perch_traced_with(parent: &TempDir, mut command: Command) -> (Output, Vec<String>) {
    let trace = parent.path().join("git-trace.log");
    let output = command
        .env("PERCH_NO_HOOKS", "1")
        .env("GIT_TRACE", &trace)
        .output()
        .expect("failed to run perch");
    let fetches = fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("git fetch"))
        .map(str::to_string)
        .collect();
    (output, fetches)
}

/// A branch the remote has that this clone has never fetched is what the
/// prefetch is for: resolved against the refs from before it, the name looks
/// new and gets a fresh branch off the default instead of the remote's commits.
#[test]
fn wt_creates_a_worktree_for_a_remote_branch_this_clone_has_never_fetched() {
    let (bare, parent, work) = setup_with_parent();
    let other = clone_bare(bare.path());
    git(other.path(), &["switch", "-c", "feat/x"]);
    commit_in(other.path(), "x.txt", "on feat/x");
    git(other.path(), &["push", "origin", "feat/x"]);
    let tip = remote_branch_tip(&work, "origin", "feat/x").unwrap();

    let output = perch_args(&work, &["wt", "feat/x", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let path = parent.path().join("worktrees").join("repo").join("feat/x");
    let head = git(&path, &["rev-parse", "HEAD"]);
    assert_eq!(
        stdout_str(&head).trim(),
        tip,
        "worktree is not at the remote tip"
    );
    let upstream = git(&path, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
    assert_eq!(stdout_str(&upstream).trim(), "origin/feat/x");
}

#[test]
fn wt_updates_an_existing_worktree_with_a_single_fetch() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree(&work, &parent, "feature");

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 1, "fetches: {fetches:?}");
}

#[test]
fn wt_creates_a_worktree_with_a_single_fetch_when_the_branch_shares_the_remote() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 1, "fetches: {fetches:?}");
}

/// The prefetch covers the current branch's remote and no other, so a branch
/// tracking a second remote is still fetched from there before its worktree is
/// made, as it was before the prefetch existed.
#[test]
fn wt_fetches_the_branch_remote_too_when_it_is_not_the_one_prefetched() {
    let (_bare, parent, work) = setup_with_parent();
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "--bare"]);
    git(
        &work,
        &[
            "remote",
            "add",
            "upstream",
            upstream.path().to_str().unwrap(),
        ],
    );
    git(&work, &["branch", "feature"]);
    git(&work, &["push", "-u", "upstream", "feature"]);

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 2, "fetches: {fetches:?}");
    assert!(fetches[0].ends_with("origin"), "fetches: {fetches:?}");
    assert!(fetches[1].ends_with("upstream"), "fetches: {fetches:?}");
}

/// Two worktrees can point one remote name at the same URL and still fetch
/// different refs, since the refspec is per-worktree config like any other.
/// A prefetch that brought back only `main` covers nothing the target worktree
/// needs, so its update must still fetch for itself.
#[test]
fn wt_still_fetches_a_worktree_whose_origin_fetches_other_refs() {
    let (bare, parent, work) = setup_with_parent();
    let worktree = add_worktree(&work, &parent, "feature");
    git(&work, &["push", "origin", "feature"]);
    git(&work, &["fetch", "--prune", "origin"]);
    git(&work, &["config", "core.repositoryFormatVersion", "1"]);
    git(&work, &["config", "extensions.worktreeConfig", "true"]);
    // The invoking worktree fetches only `main`; the target fetches the lot.
    git(&work, &["config", "--unset-all", "remote.origin.fetch"]);
    git(
        &work,
        &[
            "config",
            "--worktree",
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
    );
    git(
        &worktree,
        &[
            "config",
            "--worktree",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    // Advance `feature` on the remote behind both worktrees' backs.
    let pusher = clone_bare(bare.path());
    git(pusher.path(), &["switch", "feature"]);
    commit_in(pusher.path(), "ahead.txt", "ahead");
    git(pusher.path(), &["push", "origin", "feature"]);
    let tip = remote_branch_tip(&work, "origin", "feature").unwrap();

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 2, "fetches: {fetches:?}");
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        stdout_str(&head).trim(),
        tip,
        "the worktree was not brought up to its own origin/feature"
    );
}

/// Settings outside `remote.<name>.*` shape a fetch too: with `fetch.pruneTags`
/// the target worktree's own fetch prunes tags the remote no longer has, which
/// the invoking worktree's prefetch does not. Under `GIT_CONFIG`, `git config`
/// reads only that file and so reports the same config everywhere, while
/// `git fetch` ignores it; the comparison has to read what the fetch reads.
#[test]
fn wt_still_fetches_a_worktree_whose_fetch_config_differs() {
    for git_config in [false, true] {
        let (_bare, parent, work) = setup_with_parent();
        let worktree = add_worktree(&work, &parent, "feature");
        git(&work, &["push", "origin", "feature"]);
        git(&work, &["config", "core.repositoryFormatVersion", "1"]);
        git(&work, &["config", "extensions.worktreeConfig", "true"]);
        git(
            &worktree,
            &["config", "--worktree", "fetch.pruneTags", "true"],
        );
        // A tag the remote never had, so the target's own fetch prunes it.
        git(&work, &["tag", "stale"]);
        let mut command = perch_command(&work, &["wt", "feature", "--no-switch"]);
        if git_config {
            command.env("GIT_CONFIG", work.join(".git/config"));
        }

        let (output, fetches) = perch_traced_with(&parent, command);

        assert!(
            output.status.success(),
            "GIT_CONFIG {git_config}: stderr: {}",
            stderr_str(&output)
        );
        assert_eq!(
            fetches.len(),
            2,
            "GIT_CONFIG {git_config}: fetches: {fetches:?}"
        );
        let tags = git(&work, &["tag", "--list", "stale"]);
        assert_eq!(
            stdout_str(&tags).trim(),
            "",
            "GIT_CONFIG {git_config}: the stale tag was not pruned"
        );
    }
}

/// A relative URL has the same text in every worktree, but git resolves it
/// against the directory it runs in, so `../origin.git` names a different
/// repository from each. A transport helper can read its address the same way,
/// as `ext::` does, and `remote.<name>.vcs` hands the fetch to a helper
/// whatever the URL says, or with no URL at all. A transport program named by
/// a relative path is found from where it runs too, whether config or the
/// environment names it, and so is a bare `ssh` found through a relative or
/// empty `PATH` entry, or a relative `GIT_EXEC_PATH`, which git puts first on
/// `PATH`. Config and URL can both match while the fetches do not.
#[test]
fn wt_still_fetches_a_worktree_whose_origin_resolves_from_where_it_runs() {
    // An `ssh://` remote whose `ssh` is the wrapper written below.
    const SSH_REMOTE: [(&str, &str); 2] = [
        ("remote.origin.url", "ssh://example.invalid/repo.git"),
        ("ssh.variant", "simple"),
    ];
    for settings in [
        &[("remote.origin.url", "../origin.git")][..],
        &[("remote.origin.url", "ext::git %s ../origin.git")],
        &[
            ("remote.origin.url", "file://../origin.git"),
            ("remote.origin.vcs", "relative"),
        ],
        &[("remote.origin.url", "relative://../origin.git")],
        &[("remote.origin.vcs", "relative")],
        &[("core.sshCommand", "./ssh"), SSH_REMOTE[0], SSH_REMOTE[1]],
        &[
            ("core.sshCommand", "env WRAPPED=1 ./ssh"),
            SSH_REMOTE[0],
            SSH_REMOTE[1],
        ],
        &[("GIT_EXEC_PATH", "."), SSH_REMOTE[0], SSH_REMOTE[1]],
        &[("GIT_SSH_COMMAND", "./ssh"), SSH_REMOTE[0], SSH_REMOTE[1]],
        // A `PATH` setting goes in front of the test's own `PATH`.
        &[("PATH", "."), SSH_REMOTE[0], SSH_REMOTE[1]],
        &[("PATH", ""), SSH_REMOTE[0], SSH_REMOTE[1]],
    ] {
        let case = format!("{settings:?}");
        // A key with no `.` is an environment variable, where config has none.
        let (config, env): (Vec<_>, Vec<_>) =
            settings.iter().partition(|(key, _)| key.contains('.'));
        let (path_front, env): (Vec<_>, Vec<_>) =
            env.into_iter().partition(|(key, _)| *key == "PATH");
        let (bare, parent, work) = setup_with_parent();
        let worktree = add_worktree(&work, &parent, "feature");
        git(&work, &["push", "origin", "feature"]);
        let near = parent.path().join("origin.git");
        let far = parent.path().join("worktrees/repo/origin.git");
        for clone in [&near, &far] {
            git(
                parent.path(),
                &[
                    "clone",
                    "--bare",
                    bare.path().to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
        }
        // Advance `feature` only in the repository the target worktree resolves.
        let pusher = clone_bare(&far);
        git(pusher.path(), &["switch", "feature"]);
        commit_in(pusher.path(), "ahead.txt", "ahead");
        git(pusher.path(), &["push", "origin", "feature"]);
        let tip = stdout_str(&git(&far, &["rev-parse", "feature"]));
        git(&work, &["config", "--unset", "remote.origin.url"]);
        for (key, value) in config {
            git(&work, &["config", key, value]);
        }
        git(&work, &["config", "protocol.ext.allow", "always"]);
        // An ssh of the user's own in each worktree, serving the repository
        // its relative path reaches from there. Ignored, so the worktrees stay
        // clean.
        fs::write(work.join(".git/info/exclude"), "ssh\n").unwrap();
        for dir in [&work, &worktree] {
            let wrapper = dir.join("ssh");
            fs::write(&wrapper, "#!/bin/sh\nexec git upload-pack ../origin.git\n").unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let helpers = relative_remote_helper(&parent);
        let path = std::env::join_paths(
            path_front
                .iter()
                .map(|(_, entry)| PathBuf::from(entry))
                .chain(std::iter::once(helpers))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut command = perch_command(&work, &["wt", "feature", "--no-switch"]);
        command.env("PATH", path).envs(env.iter().copied());

        let (output, fetches) = perch_traced_with(&parent, command);

        assert!(
            output.status.success(),
            "{case}: stderr: {}",
            stderr_str(&output)
        );
        assert_eq!(fetches.len(), 2, "{case}: fetches: {fetches:?}");
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        assert_eq!(
            stdout_str(&head).trim(),
            tip.trim(),
            "{case}: the worktree was not brought up to its own origin/feature"
        );
    }
}

/// A helper of the user's own for `relative::` remotes, in a directory of its
/// own under `parent` for the caller to put on `PATH`. It reads its address as
/// a path from where it runs, as an `ext::` URL does. Where the remote has no
/// URL, git passes its name instead, and the helper falls back to the path.
fn relative_remote_helper(parent: &TempDir) -> PathBuf {
    let helpers = parent.path().join("helpers");
    fs::create_dir(&helpers).unwrap();
    let helper = helpers.join("git-remote-relative");
    fs::write(
        &helper,
        "#!/bin/sh\ncase \"$2\" in\n  *://*) address=\"${2#*://}\" ;;\n  *) address=../origin.git ;;\nesac\nexec git remote-ext \"$1\" \"git %s $address\"\n",
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    helpers
}

/// The prefetch covers a remote *name* as the invoking worktree resolves it.
/// With `extensions.worktreeConfig` the same name can point elsewhere from
/// another worktree, and that worktree's update must still fetch from where
/// its own `origin` goes, as it did before the prefetch existed.
#[test]
fn wt_still_fetches_a_worktree_whose_origin_points_elsewhere() {
    let (bare, parent, work) = setup_with_parent();
    let elsewhere = TempDir::new().unwrap();
    git(elsewhere.path(), &["init", "--bare"]);
    let worktree = add_worktree(&work, &parent, "feature");
    // Each worktree carries its own `origin` URL and the shared config none.
    git(&work, &["config", "core.repositoryFormatVersion", "1"]);
    git(&work, &["config", "extensions.worktreeConfig", "true"]);
    git(&work, &["config", "--unset", "remote.origin.url"]);
    git(
        &work,
        &[
            "config",
            "--worktree",
            "remote.origin.url",
            bare.path().to_str().unwrap(),
        ],
    );
    git(
        &worktree,
        &[
            "config",
            "--worktree",
            "remote.origin.url",
            elsewhere.path().to_str().unwrap(),
        ],
    );
    commit_in(&worktree, "b.txt", "only elsewhere");
    git(&worktree, &["push", "origin", "feature"]);
    git(&worktree, &["reset", "--hard", "HEAD~1"]);
    let tip = remote_branch_tip(&worktree, "origin", "feature").unwrap();

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 2, "fetches: {fetches:?}");
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        stdout_str(&head).trim(),
        tip,
        "the worktree was not updated from its own origin"
    );
}

/// Each worktree keeps its own submodule repositories, and a fetch recurses on
/// demand only into those where it runs, for gitlinks moved by the commits it
/// fetched. A prefetch that had already moved the shared remote refs would
/// leave the target's own fetch nothing new to recurse for. So a repository
/// with submodules skips the prefetch, and the target fetches for itself, once.
/// A worktree that gains submodules only once the prefetch is under way has
/// its own fetch recurse into all of them. Either way its submodule gets the
/// commit its advanced gitlink names.
#[test]
fn wt_still_fetches_a_worktree_with_submodules() {
    // The mode git records a submodule's commit under in its superproject.
    const GITLINK_MODE: &str = "160000";
    for gained_mid_run in [false, true] {
        let case = format!("gained mid-run {gained_mid_run}");
        let (bare, parent, work) = setup_with_parent();
        let submodule = TempDir::new().unwrap();
        git(submodule.path(), &["init", "--initial-branch=main"]);
        commit_in(submodule.path(), "s.txt", "sub initial");
        // Submodules clone and fetch over the file transport only when allowed.
        let allow_file = ["-c", "protocol.file.allow=always"];
        let url = submodule.path().to_str().unwrap();
        git(
            &work,
            &[&allow_file[..], &["submodule", "add", url, "sub"]].concat(),
        );
        git(&work, &["commit", "-m", "add sub"]);
        git(&work, &["push", "origin", "main"]);
        let worktree = add_worktree(&work, &parent, "feature");
        git(&work, &["push", "origin", "feature"]);
        git(
            &worktree,
            &[&allow_file[..], &["submodule", "update", "--init"]].concat(),
        );
        // Advance the submodule, and the gitlink on the remote's `feature` to
        // match.
        commit_in(submodule.path(), "s2.txt", "sub ahead");
        let sub_tip = stdout_str(&git(submodule.path(), &["rev-parse", "HEAD"]))
            .trim()
            .to_string();
        let pusher = clone_bare(bare.path());
        git(pusher.path(), &["switch", "feature"]);
        git(
            pusher.path(),
            &[
                "update-index",
                "--cacheinfo",
                &format!("{GITLINK_MODE},{sub_tip},sub"),
            ],
        );
        git(pusher.path(), &["commit", "-m", "advance sub"]);
        git(pusher.path(), &["push", "origin", "feature"]);
        if gained_mid_run {
            // Hidden from the startup scan, and put back as the prefetch
            // commits its ref updates, before the target's own fetch.
            let mut restore = String::new();
            for (name, dir) in [("work", &work), ("feature", &worktree)] {
                let hidden = parent.path().join(format!("{name}.gitmodules"));
                let gitmodules = dir.join(".gitmodules");
                fs::rename(&gitmodules, &hidden).unwrap();
                writeln!(
                    restore,
                    "[ -f '{0}' ] && mv '{0}' '{1}'",
                    hidden.display(),
                    gitmodules.display(),
                )
                .unwrap();
            }
            let hook = work.join(".git/hooks/reference-transaction");
            fs::write(
                &hook,
                format!(
                    "#!/bin/sh\ncat >/dev/null\n[ \"$1\" = committed ] || exit 0\n{restore}exit 0\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut command = perch_command(&work, &["wt", "feature", "--no-switch"]);
        // `allow_file` for perch's own git, including the submodule fetches.
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
            .env("GIT_CONFIG_VALUE_0", "always");

        let (output, fetches) = perch_traced_with(&parent, command);

        assert!(
            output.status.success(),
            "{case}: stderr: {}",
            stderr_str(&output)
        );
        // The trace also carries each fetch's recursion into its submodule.
        let own = fetches
            .iter()
            .filter(|line| line.contains("git fetch --quiet --prune origin"))
            .count();
        let expected = if gained_mid_run { 2 } else { 1 };
        assert_eq!(own, expected, "{case}: fetches: {fetches:?}");
        // Fails unless the target's submodule repository holds the new commit.
        git(
            &worktree.join("sub"),
            &["cat-file", "-e", &format!("{sub_tip}^{{commit}}")],
        );
    }
}

/// A relative `core.hooksPath` resolves from each worktree's root, so each
/// fetch can run a different `reference-transaction` hook.
#[test]
fn wt_still_fetches_a_worktree_under_a_relative_hooks_path() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree(&work, &parent, "feature");
    git(&work, &["push", "origin", "feature"]);
    git(&work, &["config", "core.hooksPath", "hooks"]);

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 2, "fetches: {fetches:?}");
}

/// A variable that picks the repository overrides it for both fetches alike,
/// so there is no tip to compare, only the fetch that coverage must not skip.
#[test]
fn wt_still_fetches_a_worktree_when_the_environment_picks_the_repository() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree(&work, &parent, "feature");
    git(&work, &["push", "origin", "feature"]);
    let mut command = perch_command(&work, &["wt", "feature", "--no-switch"]);
    command.env("GIT_COMMON_DIR", work.join(".git"));

    let (output, fetches) = perch_traced_with(&parent, command);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 2, "fetches: {fetches:?}");
}

/// A remote that cannot be reached is worth one warning, not one per attempt,
/// and no reason to refuse a new branch: making one offline is legitimate.
#[test]
fn wt_reports_an_unreachable_remote_once_and_still_creates_the_branch() {
    let (_bare, parent, work) = setup_with_parent();
    git(
        &work,
        &["remote", "set-url", "origin", "/nonexistent/nowhere"],
    );

    let output = perch_args(&work, &["wt", "brand-new", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let stderr = stderr_str(&output);
    assert_eq!(
        stderr.matches("fetch failed; results may be stale").count(),
        1,
        "stderr: {stderr}"
    );
    assert!(
        stderr.find("fetch failed").unwrap() < stderr.find("created brand-new").unwrap(),
        "the fetch failure must be reported before the creation: {stderr}"
    );
    let path = parent
        .path()
        .join("worktrees")
        .join("repo")
        .join("brand-new");
    assert!(path.is_dir(), "missing worktree: {}", path.display());
}

/// Each worktree has its own `FETCH_HEAD`, so a fetch that fails from one can
/// succeed from another, and the prefetch failing covers nothing there.
#[test]
fn wt_still_fetches_a_worktree_the_prefetch_failed_to_cover() {
    let (bare, parent, work) = setup_with_parent();
    let worktree = add_worktree(&work, &parent, "feature");
    git(&work, &["push", "origin", "feature"]);
    let pusher = clone_bare(bare.path());
    git(pusher.path(), &["switch", "feature"]);
    commit_in(pusher.path(), "ahead.txt", "ahead");
    git(pusher.path(), &["push", "origin", "feature"]);
    let tip = remote_branch_tip(&work, "origin", "feature").unwrap();
    // A directory where the invoking worktree's `FETCH_HEAD` goes makes every
    // fetch from there fail, and none from the target.
    fs::create_dir(work.join(".git/FETCH_HEAD")).unwrap();

    let (output, fetches) = perch_traced(&parent, &work, &["wt", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(fetches.len(), 3, "fetches: {fetches:?}");
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        stdout_str(&head).trim(),
        tip,
        "the worktree was not brought up to its own origin/feature"
    );
}

/// Updating a worktree fetches again where the prefetch failed, and where that
/// fails the same way the warning has already been printed.
#[test]
fn wt_reports_an_unreachable_remote_once_when_updating_a_worktree() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree(&work, &parent, "feature");
    git(
        &work,
        &["remote", "set-url", "origin", "/nonexistent/nowhere"],
    );

    let output = perch_args(&work, &["wt", "feature", "--no-switch"]);

    let stderr = stderr_str(&output);
    assert_eq!(
        stderr.matches("fetch failed; results may be stale").count(),
        1,
        "stderr: {stderr}"
    );
}

#[test]
fn wt_no_switch_is_rejected_before_rm() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");

    let output = perch_args(&path, &["wt", "--no-switch", "rm", ".", "--force"]);

    assert!(
        !output.status.success(),
        "the create-only option must not reach `wt rm`; stderr: {}",
        stderr_str(&output)
    );
    assert_eq!(
        stdout_str(&output),
        "",
        "a rejected option must not hand the shell a directory"
    );
    assert!(path.exists(), "worktree must survive: {}", path.display());
}

#[test]
fn wt_double_dash_stops_no_switch_option_parsing() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let output = perch_args(&work, &["wt", "--", "feature", "--no-switch"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let expected = parent.path().join("worktrees/repo/feature");
    assert_eq!(
        Path::new(stdout_str(&output).trim())
            .canonicalize()
            .unwrap(),
        expected.canonicalize().unwrap(),
        "an option after `--` must not suppress the shell handoff"
    );
}

#[test]
fn wt_preserves_slashes_as_subdirs() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature/nested"]);

    let output = perch_args(&work, &["wt", "feature/nested"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let expected = parent
        .path()
        .join("worktrees")
        .join("repo")
        .join("feature")
        .join("nested");
    assert!(
        expected.exists(),
        "nested worktree should exist at {}",
        expected.display()
    );
}

#[test]
fn wt_cd_to_existing_worktree_prints_path() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    let output = perch_args(&work, &["wt", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let printed = stdout_str(&output).trim().to_string();
    assert!(
        printed.ends_with("worktrees/repo/feature") && Path::new(&printed).is_dir(),
        "stdout should be the existing worktree path; got: {printed}"
    );
    // A worktree branch without an upstream must not emit "No remote…" noise.
    assert!(
        !stderr_str(&output).contains("No remote"),
        "cd to a worktree should stay quiet about missing upstream; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn wt_refuses_when_target_path_is_stale_non_worktree_directory() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let stale = parent.path().join("worktrees").join("repo").join("feature");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("leftover.txt"), "junk").unwrap();

    let output = perch_args(&work, &["wt", "feature"]);
    assert!(!output.status.success());

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("exists but is not a registered worktree"),
        "expected stale-dir error; got: {combined}"
    );
}

#[test]
fn wt_recreates_worktree_whose_directory_was_deleted_by_hand() {
    let (_bare, parent, work) = setup_with_parent();

    // Create a worktree, then delete its directory without telling git. The
    // registration lingers as "missing but already registered" and would block
    // `git worktree add`; perch should prune it and recreate cleanly.
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "-b", "feature"],
    );
    fs::remove_dir_all(&path).unwrap();

    let output = perch_args(&work, &["wt", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        path.is_dir(),
        "worktree should be recreated at {}",
        path.display()
    );

    let list = stdout_str(&git(&work, &["worktree", "list", "--porcelain"]));
    assert!(
        !list.contains("prunable"),
        "stale registration should be pruned; got: {list}"
    );
}

#[test]
fn wt_ls_lists_all_worktrees() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    let output = perch_args(&work, &["wt", "ls"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let out = stdout_str(&output);
    assert!(out.contains("main"), "ls should mention main; got: {out}");
    assert!(
        out.contains("feature"),
        "ls should mention feature; got: {out}"
    );
}

#[test]
fn wt_rm_removes_worktree_and_deletes_branch() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    let output = perch_args(&work, &["wt", "rm", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    assert!(
        !path.exists(),
        "worktree dir should be removed: {}",
        path.display()
    );

    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        !stdout_str(&branches).lines().any(|l| l == "feature"),
        "branch should be deleted; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn wt_rm_rejects_malformed_invocations_before_removing_anything() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    let cases: &[(&[&str], &str)] = &[
        (
            &["wt", "rm", "--remote", "feature"],
            "unknown option '--remote'",
        ),
        (
            &["wt", "rm", "-f", "--force", "feature"],
            "duplicate option '--force'",
        ),
        (
            &["wt", "rm", "--", "feature", "other"],
            "unexpected extra target 'other'",
        ),
    ];

    for (args, expected) in cases {
        let output = perch_args(&work, args);
        assert!(!output.status.success(), "{args:?} should fail");
        assert!(
            stderr_str(&output).contains(expected),
            "{args:?} should report {expected:?}; got: {}",
            stderr_str(&output)
        );
        assert!(path.exists(), "{args:?} must not remove the worktree");
    }
}

#[test]
fn wt_rm_double_dash_allows_a_target_named_like_an_option() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("--force");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    let output = perch_args(&work, &["wt", "rm", "--force", "--", "--force"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!path.exists(), "the escaped target should be removed");
}

/// Risk is judged from the main worktree, so the delete must run there too.
/// `git branch -d` consults HEAD only where no upstream is set, so an untracked
/// branch is where the difference shows: removing it while standing in an
/// unrelated worktree used to ask `-d` from that unrelated HEAD, which refuses.
/// The row was marked safe and the branch survived anyway.
#[test]
fn wt_rm_deletes_an_untracked_merged_branch_from_an_unrelated_worktree() {
    let (_bare, parent, work) = setup_with_parent();

    let done = parent
        .path()
        .join("worktrees")
        .join("repo")
        .join("feature-done");
    fs::create_dir_all(done.parent().unwrap()).unwrap();
    git(
        &work,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "feature-done",
            done.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(done.join("done.txt"), "work\n").unwrap();
    git(&done, &["add", "done.txt"]);
    git(&done, &["commit", "-m", "done"]);
    git(&work, &["merge", "--ff-only", "feature-done"]);

    // Diverge the worktree we run from, so `feature-done` is merged into main
    // but not into this HEAD.
    let elsewhere = add_worktree_branch(&work, parent.path(), "feature-elsewhere");
    fs::write(elsewhere.join("other.txt"), "other\n").unwrap();
    git(&elsewhere, &["add", "other.txt"]);
    git(&elsewhere, &["commit", "-m", "other"]);

    let output = perch_args(&elsewhere, &["wt", "rm", "feature-done"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        !stdout_str(&branches).lines().any(|l| l == "feature-done"),
        "branch merged into main should be deleted; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn in_place_switch_hands_off_when_branch_is_held_by_worktree() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    // Plain `perch feature` from main worktree: branch is held → handoff.
    let output = perch(&work, "feature");
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let printed = stdout_str(&output).trim().to_string();
    assert!(
        printed.ends_with("worktrees/repo/feature") && Path::new(&printed).is_dir(),
        "stdout should be the worktree path for the shell wrapper; got: {printed}"
    );

    // Original worktree's HEAD must NOT have changed (no checkout happened).
    let head = git(&work, &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "main");
}

#[test]
fn br_checks_the_branch_out_in_the_current_worktree() {
    let (_bare, work) = setup();

    git(work.path(), &["branch", "feature"]);

    let output = perch_args(work.path(), &["br", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let head = git(work.path(), &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "feature");
}

/// The message is the feature: it's where `br` teaches the verb that does
/// reach a branch another worktree holds.
#[test]
fn br_refuses_a_held_branch_and_names_the_verb_that_reaches_it() {
    let (_bare, parent, work) = setup_with_parent();

    add_worktree(&work, &parent, "feature");

    let output = perch_args(&work, &["br", "feature"]);
    assert!(
        !output.status.success(),
        "br into a held branch should fail; stderr: {}",
        stderr_str(&output)
    );

    // The path may come back with `$HOME` abbreviated to `~`, so match its tail.
    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("worktrees/repo/feature"),
        "error should name the worktree holding the branch; got: {stderr}"
    );
    assert!(
        stderr.contains("run `perch feature` to go there"),
        "error should point at the verb that reaches it; got: {stderr}"
    );

    // No handoff: `br` never prints a path for the shell wrapper to `cd` into.
    assert_eq!(stdout_str(&output).trim(), "");

    let head = git(&work, &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "main");
}

/// A branch named after a verb is read as that verb, so the advice `br` gives
/// has to route around the dispatcher or it lands somewhere else entirely.
#[test]
fn br_points_a_verb_named_branch_at_the_escape_hatch() {
    for verb in ["br", "wt"] {
        let (_bare, parent, work) = setup_with_parent();
        add_worktree(&work, &parent, verb);

        let output = perch_args(&work, &["br", verb]);
        assert!(
            !output.status.success(),
            "br into a held branch should fail; stderr: {}",
            stderr_str(&output)
        );
        assert!(
            stderr_str(&output).contains(&format!("run `perch -- {verb}` to go there")),
            "a branch named `{verb}` needs the `--` form; got: {}",
            stderr_str(&output)
        );
    }
}

/// `wt <name>` creates a worktree for any word it doesn't know, so a retired
/// subverb left to fall through would build a branch called `list`.
#[test]
fn a_retired_wt_subverb_is_refused_rather_than_taken_for_a_branch() {
    let (_bare, _parent, work) = setup_with_parent();

    for (retired, keep) in [("list", "wt ls"), ("remove", "wt rm")] {
        let output = perch_args(&work, &["wt", retired]);
        assert!(
            !output.status.success(),
            "`wt {retired}` should fail; stderr: {}",
            stderr_str(&output)
        );
        assert!(
            stderr_str(&output).contains(&format!("use `perch {keep}`")),
            "error should name the spelling that replaced it; got: {}",
            stderr_str(&output)
        );
        assert!(
            stderr_str(&output).contains(&format!("`perch wt -- {retired}`")),
            "error should name the escape hatch for a branch by that name; got: {}",
            stderr_str(&output)
        );

        let branches = git(&work, &["branch", "--format=%(refname:short)"]);
        assert!(
            !stdout_str(&branches).lines().any(|l| l == retired),
            "`wt {retired}` must not create a branch; got: {}",
            stdout_str(&branches)
        );
    }
}

/// `--` ends subverb parsing, which is the only way left to name a branch that
/// collides with a subverb — `wt list` is the retired-spelling error, not a
/// branch.
#[test]
fn a_wt_subverb_named_branch_is_reachable_past_the_dispatcher() {
    for name in ["list", "remove", "ls", "rm"] {
        let (_bare, _parent, work) = setup_with_parent();
        git(&work, &["branch", name]);

        let output = perch_args(&work, &["wt", "--", name]);
        assert!(
            output.status.success(),
            "`wt -- {name}` should worktree the branch; stderr: {}",
            stderr_str(&output)
        );

        let worktrees = git(&work, &["worktree", "list", "--porcelain"]);
        assert!(
            stdout_str(&worktrees).contains(&format!("branch refs/heads/{name}")),
            "`wt -- {name}` should hold branch `{name}`; got: {}",
            stdout_str(&worktrees)
        );
    }
}

/// `br` reads `rm` as a subverb, so `--` is how a branch with that exact name
/// remains reachable.
#[test]
fn br_takes_the_branch_after_a_double_dash() {
    let (_bare, work) = setup();

    git(work.path(), &["branch", "rm"]);

    let output = perch_args(work.path(), &["br", "--", "rm"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let head = git(work.path(), &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "rm");
}

#[test]
fn br_rm_deletes_a_merged_local_branch() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);

    let output = perch_args(work.path(), &["br", "rm", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "feature"));
    assert!(
        stderr_str(&output).contains("deleted feature"),
        "the removal should be reported; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_refuses_an_unmerged_branch_non_interactively() {
    let (_bare, work) = setup();
    git(work.path(), &["switch", "-c", "feature"]);
    commit_in(work.path(), "feature.txt", "feature work");
    git(work.path(), &["switch", "main"]);

    let output = perch_args(work.path(), &["br", "rm", "feature"]);

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "feature"));
    assert!(
        stderr_str(&output).contains("pass --force"),
        "the refusal should name the non-interactive escape hatch; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_force_deletes_an_unmerged_branch() {
    let (_bare, work) = setup();
    git(work.path(), &["switch", "-c", "feature"]);
    commit_in(work.path(), "feature.txt", "feature work");
    git(work.path(), &["switch", "main"]);

    let output = perch_args(work.path(), &["br", "rm", "feature", "--force"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "feature"));
}

#[test]
fn br_rm_refuses_a_branch_held_by_a_worktree() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");

    let output = perch_args(&work, &["br", "rm", "feature", "--force"]);

    assert!(!output.status.success());
    assert!(local_branch_exists(&work, "feature"));
    assert!(path.is_dir());
    assert!(
        stderr_str(&output).contains("perch wt rm feature"),
        "held branches should point to the owner of worktree removal; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_refuses_the_main_worktree_branch_without_a_wt_rm_hint() {
    let (_bare, work) = setup();

    let output = perch_args(work.path(), &["br", "rm", "main", "--force"]);
    let stderr = stderr_str(&output);

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "main"));
    assert!(
        stderr.contains("check out another branch in the main worktree first"),
        "the refusal should explain how to release the main-worktree branch; got: {stderr}",
    );
    assert!(
        !stderr.contains("wt rm"),
        "the main worktree cannot be removed with wt rm; got: {stderr}",
    );
}

#[test]
fn br_rm_does_not_give_dot_a_special_meaning() {
    let (_bare, work) = setup();

    let output = perch_args(work.path(), &["br", "rm", "."]);

    assert!(!output.status.success());
    assert!(
        stderr_str(&output).contains("branch '.' does not exist locally"),
        "dot should be handled as an ordinary branch name; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_rejects_unknown_options_and_extra_targets() {
    let (_bare, work) = setup();

    let unknown = perch_args(work.path(), &["br", "rm", "--remote", "feature"]);
    assert!(!unknown.status.success());
    assert!(stderr_str(&unknown).contains("unknown option '--remote'"));

    let extra = perch_args(work.path(), &["br", "rm", "one", "two"]);
    assert!(!extra.status.success());
    assert!(stderr_str(&extra).contains("unexpected extra target 'two'"));

    let short_force = perch_args(work.path(), &["br", "rm", "feature", "-f"]);
    assert!(!short_force.status.success());
    assert!(stderr_str(&short_force).contains("unknown option '-f'"));
}

#[test]
fn br_rm_picker_shows_but_does_not_select_disabled_branches() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "free"]);
    git(&work, &["branch", "held"]);
    git(&work, &["switch", "-c", "current"]);
    let held_path = parent.path().join("held-worktree");
    git(
        &work,
        &["worktree", "add", held_path.to_str().unwrap(), "held"],
    );

    let output = String::from_utf8_lossy(&drive_multi_select_prompt(
        &work,
        &["br", "rm", "--force"],
        "free",
        false,
        || {},
    ))
    .into_owned();

    assert!(!local_branch_exists(&work, "free"));
    for branch in ["current", "held"] {
        assert!(
            local_branch_exists(&work, branch),
            "disabled branch {branch} should survive; output: {output}"
        );
    }
    assert!(output.contains("current"), "current row missing: {output}");
    assert!(
        output.contains("use wt rm"),
        "held row should point to wt rm: {output}"
    );
}

/// Keeping is about the sweep, so `br rm`'s picker draws a kept branch and the
/// local default branch as ordinary rows — select-all reaches both.
#[test]
fn br_rm_picker_offers_a_kept_branch_and_the_default_branch() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "kept"]);
    git(work.path(), &["config", "--add", "perch.keep", "kept"]);
    git(work.path(), &["switch", "-c", "topic"]);

    let output = String::from_utf8_lossy(&drive_multi_select_prompt(
        work.path(),
        &["br", "rm", "--force"],
        "kept",
        false,
        || {},
    ))
    .into_owned();

    assert!(
        !local_branch_exists(work.path(), "kept"),
        "a kept branch is an ordinary picker row; output: {output}"
    );
    assert!(
        !local_branch_exists(work.path(), "main"),
        "the local default branch is an ordinary picker row; output: {output}"
    );
}

#[test]
fn br_rm_named_local_default_branch_is_removable_when_unheld() {
    let (_bare, work) = setup();
    git(work.path(), &["switch", "-c", "topic"]);

    let output = perch_args(work.path(), &["br", "rm", "main"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "main"));
}

#[test]
fn br_rm_completion_lists_local_branches_only() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "local"]);
    git(work.path(), &["branch", "remote-only"]);
    git(work.path(), &["push", "origin", "remote-only"]);
    git(work.path(), &["branch", "-D", "remote-only"]);

    let output = perch_args(work.path(), &["br", "rm", "--complete"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let mut branches: Vec<_> = stdout_str(&output).lines().map(String::from).collect();
    branches.sort();
    assert_eq!(branches, ["local", "main"]);
}

#[test]
fn br_rm_upstream_deletes_an_explicit_same_named_upstream() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "feature"));
    assert_eq!(remote_branch_tip(work.path(), "origin", "feature"), None);
    assert!(
        stderr_str(&output).contains("deleted upstream origin/feature"),
        "the upstream deletion should be reported; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_upstream_uses_the_configured_non_origin_remote() {
    let (_bare, work) = setup_with_remote("upstream");
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "upstream", "feature"]);

    let output = perch_args(
        work.path(),
        &["br", "rm", "--upstream", "feature", "--force"],
    );

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert_eq!(remote_branch_tip(work.path(), "upstream", "feature"), None);
}

#[test]
fn br_rm_upstream_refuses_a_mismatched_tracking_branch_before_local_deletion() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(
        work.path(),
        &["branch", "--set-upstream-to=origin/main", "feature"],
    );

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "feature"));
    assert!(remote_branch_tip(work.path(), "origin", "main").is_some());
    assert!(
        stderr_str(&output).contains("no explicit same-named upstream"),
        "a base branch must not become an inferred delete target; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_upstream_refuses_an_untracked_branch_before_local_deletion() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "feature"));
    assert!(stderr_str(&output).contains("no explicit same-named upstream"));
}

#[test]
fn br_rm_upstream_treats_an_already_absent_ref_as_finished() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    git(work.path(), &["push", "origin", "--delete", "feature"]);

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "feature"));
    assert!(
        stderr_str(&output).contains("upstream origin/feature is already absent"),
        "the absent remote half should not become an error; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_upstream_never_deletes_the_remote_default_branch() {
    let (_bare, work) = setup();
    git(work.path(), &["switch", "-c", "feature"]);

    let output = perch_args(work.path(), &["br", "rm", "main", "--upstream", "--force"]);

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "main"));
    assert!(remote_branch_tip(work.path(), "origin", "main").is_some());
    assert!(
        stderr_str(&output).contains("remote's default branch"),
        "the protected reason should be explicit; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn br_rm_force_without_upstream_leaves_the_remote_branch_alone() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    let output = perch_args(work.path(), &["br", "rm", "feature", "--force"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!local_branch_exists(work.path(), "feature"));
    assert!(remote_branch_tip(work.path(), "origin", "feature").is_some());
}

#[test]
fn br_rm_named_upstream_confirmation_defaults_off() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    let output = drive_enter_confirmation(
        work.path(),
        &["br", "rm", "feature"],
        "Delete upstream origin/feature too?",
    );

    assert!(!local_branch_exists(work.path(), "feature"));
    assert!(remote_branch_tip(work.path(), "origin", "feature").is_some());
    assert!(output.contains("deleting origin/feature removes a shared upstream ref"));
}

#[test]
fn br_rm_upstream_flag_preselects_the_named_confirmation() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    let output = drive_enter_confirmation(
        work.path(),
        &["br", "rm", "feature", "--upstream"],
        "Delete upstream origin/feature too?",
    );

    assert!(!local_branch_exists(work.path(), "feature"));
    assert_eq!(remote_branch_tip(work.path(), "origin", "feature"), None);
    assert!(output.contains("deleted upstream origin/feature"));
}

#[test]
fn br_rm_named_upstream_confirmation_advertises_escape() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    let output = drive_escape_confirmation(
        work.path(),
        &["br", "rm", "feature", "--upstream"],
        "Delete upstream origin/feature too?",
    );

    assert!(
        output.contains("[Y/n] / esc"),
        "the confirmation should advertise its cancellation key; got: {output}",
    );
}

#[test]
fn br_rm_escape_from_named_upstream_confirmation_cancels_every_removal() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    drive_escape_confirmation(
        work.path(),
        &["br", "rm", "feature", "--upstream"],
        "Delete upstream origin/feature too?",
    );

    assert_eq!(
        (
            local_branch_exists(work.path(), "feature"),
            remote_branch_tip(work.path(), "origin", "feature").is_some(),
        ),
        (true, true),
        "Escape from the named upstream confirmation must cancel the whole removal",
    );
}

#[test]
fn br_rm_escape_from_upstream_picker_cancels_every_removal() {
    let (_bare, work) = setup();
    for branch in ["a", "b"] {
        git(work.path(), &["branch", branch]);
        git(work.path(), &["push", "-u", "origin", branch]);
    }

    drive_multi_select_prompt_then_escape(
        work.path(),
        &["br", "rm", "--upstream"],
        "a",
        "Also delete upstream branches?",
    );

    assert_eq!(
        (
            local_branch_exists(work.path(), "a"),
            local_branch_exists(work.path(), "b"),
            remote_branch_tip(work.path(), "origin", "a").is_some(),
            remote_branch_tip(work.path(), "origin", "b").is_some(),
        ),
        (true, true, true, true),
        "Escape from the upstream picker must cancel the whole removal",
    );
}

#[test]
fn br_rm_batch_keeps_a_failed_local_and_its_upstream_then_continues() {
    let (_bare, parent, work) = setup_with_parent();
    git(&work, &["branch", "a"]);
    git(&work, &["branch", "b"]);
    git(&work, &["push", "-u", "origin", "a"]);
    let held = parent.path().join("held-a");

    let output = String::from_utf8_lossy(&drive_multi_select_prompt(
        &work,
        &["br", "rm", "--upstream", "--force"],
        "a",
        false,
        || {
            git(&work, &["worktree", "add", held.to_str().unwrap(), "a"]);
        },
    ))
    .into_owned();

    assert!(local_branch_exists(&work, "a"));
    assert!(!local_branch_exists(&work, "b"));
    assert!(remote_branch_tip(&work, "origin", "a").is_some());
    assert!(
        output.contains("kept upstream origin/a because the local branch still exists"),
        "the remote half must follow local failure: {output}"
    );
    assert!(
        output.contains("one or more requested removals failed"),
        "a partial batch failure must return a failing result: {output}"
    );
    assert!(
        output.find("could not delete a").unwrap() < output.find("deleted b").unwrap(),
        "pairs should report in picker order: {output}"
    );
}

#[test]
fn br_rm_batch_continues_after_explicit_upstream_inspection_failure() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "a"]);
    git(work.path(), &["branch", "b"]);
    git(
        work.path(),
        &["remote", "add", "broken", "/path/that/does/not/exist"],
    );
    git(
        work.path(),
        &["update-ref", "refs/remotes/broken/a", "refs/heads/a"],
    );
    git(work.path(), &["config", "branch.a.remote", "broken"]);
    git(work.path(), &["config", "branch.a.merge", "refs/heads/a"]);

    let output = String::from_utf8_lossy(&drive_multi_select_prompt(
        work.path(),
        &["br", "rm", "--upstream", "--force"],
        "a",
        false,
        || {},
    ))
    .into_owned();

    assert!(local_branch_exists(work.path(), "a"));
    assert!(!local_branch_exists(work.path(), "b"));
    assert!(
        output.contains("could not prepare upstream removal for a"),
        "the failed pair should be reported in row order: {output}"
    );
    assert!(output.contains("one or more requested removals failed"));
    assert!(
        output
            .find("could not prepare upstream removal for a")
            .unwrap()
            < output.find("deleted b").unwrap(),
        "preflight failures should keep pair order: {output}"
    );
}

#[test]
fn br_rm_upstream_inspection_failure_preserves_the_local_branch() {
    let (_bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    git(
        work.path(),
        &["remote", "set-url", "origin", "/path/that/does/not/exist"],
    );

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(!output.status.success());
    assert!(local_branch_exists(work.path(), "feature"));
}

#[test]
fn br_rm_reports_partial_failure_when_the_server_refuses_upstream_deletion() {
    let (bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    git(bare.path(), &["config", "receive.denyDeletes", "true"]);

    let output = perch_args(
        work.path(),
        &["br", "rm", "feature", "--upstream", "--force"],
    );

    assert!(!output.status.success());
    assert!(!local_branch_exists(work.path(), "feature"));
    assert!(remote_branch_tip(work.path(), "origin", "feature").is_some());
    assert!(
        stderr_str(&output).contains("could not delete upstream origin/feature"),
        "the completed local half and failed remote half should be visible; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn upstream_deletion_lease_refuses_a_ref_that_moved_after_inspection() {
    let (bare, work) = setup();
    git(work.path(), &["branch", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    let shown_tip = remote_branch_tip(work.path(), "origin", "feature").unwrap();

    let other = clone_bare(bare.path());
    git(
        other.path(),
        &["switch", "--create", "feature", "origin/feature"],
    );
    commit_in(other.path(), "moved.txt", "move feature");
    git(other.path(), &["push", "origin", "feature"]);

    let _cwd = cwd_at(work.path());
    let outcome = perch::git::delete_remote_branch(&perch::git::RemoteBranch {
        remote: "origin".into(),
        branch: "feature".into(),
        tip: shown_tip.clone(),
    })
    .unwrap();

    assert!(matches!(
        outcome,
        perch::git::RemoteBranchDeleteOutcome::Moved { expected, .. }
            if expected == shown_tip
    ));
    assert!(remote_branch_tip(work.path(), "origin", "feature").is_some());
}

/// Creates a worktree for a new branch and returns its path.
fn add_worktree(work: &Path, parent: &TempDir, branch: &str) -> PathBuf {
    git(work, &["branch", branch]);
    let path = parent.path().join("worktrees").join("repo").join(branch);
    git(work, &["worktree", "add", path.to_str().unwrap(), branch]);
    path
}

fn commit_in(path: &Path, file: &str, msg: &str) {
    fs::write(path.join(file), "x\n").unwrap();
    git(path, &["add", file]);
    git(path, &["commit", "-m", msg]);
}

/// A named target carries risk but has no picker row to warn on, and a piped
/// run can neither show a warning nor ask — so it must refuse outright rather
/// than destroy something unwarned.
#[test]
fn wt_rm_refuses_unmerged_branch_non_interactively() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    commit_in(&path, "new.txt", "unmerged work");

    let output = perch_args(&work, &["wt", "rm", "feature"]);

    assert!(
        !output.status.success(),
        "should exit non-zero; stderr: {}",
        stderr_str(&output)
    );
    assert!(
        path.exists(),
        "worktree must survive a refusal: {}",
        path.display()
    );
    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("--force"),
        "refusal should point at the escape hatch; got: {stderr}"
    );

    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        stdout_str(&branches).lines().any(|l| l == "feature"),
        "branch must survive a refusal; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn wt_rm_refuses_dirty_worktree_non_interactively() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    fs::write(path.join("scratch.txt"), "uncommitted\n").unwrap();

    let output = perch_args(&work, &["wt", "rm", "feature"]);

    assert!(
        !output.status.success(),
        "should exit non-zero; stderr: {}",
        stderr_str(&output)
    );
    assert!(
        stderr_str(&output).contains("uncommitted"),
        "should name the risk; got: {}",
        stderr_str(&output)
    );
    assert!(path.exists(), "worktree must survive: {}", path.display());
}

#[test]
fn wt_rm_does_not_rename_past_gits_initialized_submodule_guard() {
    let (_bare, parent, work) = setup_with_parent();
    let submodule = TempDir::new().unwrap();
    git(submodule.path(), &["init", "--initial-branch=main"]);
    commit_in(submodule.path(), "tracked.txt", "initial submodule commit");
    git(
        &work,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            submodule.path().to_str().unwrap(),
            "module",
        ],
    );
    git(&work, &["commit", "-m", "add submodule"]);
    let path = add_worktree(&work, &parent, "feature");
    git(
        &path,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );

    let output = perch_args(&work, &["wt", "rm", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("working trees containing submodules"),
        "Git's submodule guard should speak for the refusal: {}",
        stderr_str(&output)
    );
    assert!(path.exists(), "the guarded worktree must survive");
}

#[test]
fn wt_rm_keeps_fast_reclamation_for_an_unmapped_gitlink() {
    let (_bare, parent, work) = setup_with_parent();
    let head = stdout_str(&git(&work, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},gl"),
        ],
    );
    git(&work, &["commit", "-m", "add unmapped gitlink"]);
    let path = add_worktree(&work, &parent, "feature");

    let bin = parent.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake_rm = bin.join("rm");
    fs::write(
        &fake_rm,
        "#!/bin/sh\n\
         : > \"$PERCH_TEST_RM_STARTED\"\n\
         exec /bin/rm \"$@\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_rm).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_rm, permissions).unwrap();

    let started = parent.path().join("rm-started");
    let path_env = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();

    let output = perch_command(&work, &["wt", "rm", "feature"])
        .env("PERCH_NO_HOOKS", "1")
        .env("PERCH_TEST_RM_STARTED", &started)
        .env("PATH", path_env)
        .output()
        .expect("failed to run perch");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    let reclamation_started = poll_until(|| started.exists());
    assert!(
        reclamation_started,
        "an unmapped gitlink should keep detached reclamation"
    );
    assert!(!path.exists(), "the original worktree path should be gone");
    assert!(poll_until(|| ready_trash(&path).is_none()));
}

/// `--force` waives the confirmation, discarding uncommitted changes and the
/// unmerged branch alike.
#[test]
fn wt_rm_force_removes_dirty_worktree_and_unmerged_branch() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    commit_in(&path, "new.txt", "unmerged work");
    fs::write(path.join("scratch.txt"), "uncommitted\n").unwrap();

    let output = perch_args(&work, &["wt", "rm", "feature", "--force"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    assert!(
        !path.exists(),
        "worktree should be removed: {}",
        path.display()
    );
    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        !stdout_str(&branches).lines().any(|l| l == "feature"),
        "unmerged branch should be force-deleted; got: {}",
        stdout_str(&branches)
    );
}

/// A clean, merged worktree has nothing to lose, so `.` needs no confirmation
/// even though it names a target.
#[test]
fn wt_rm_dot_removes_the_current_worktree() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");

    let output = perch_args(&path, &["wt", "rm", "."]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    assert!(
        !path.exists(),
        "the worktree we stood in should be removed: {}",
        path.display()
    );

    // The cwd just vanished, so the shell wrapper is handed the main worktree.
    let printed = stdout_str(&output).trim().to_string();
    assert_eq!(
        Path::new(&printed).canonicalize().ok(),
        work.canonicalize().ok(),
        "stdout should hand the main worktree to the shell wrapper; got: {printed}"
    );
}

/// Regression: `git branch --merged` is relative to HEAD, and every branch is
/// merged into itself — so judging risk from inside the worktree being removed
/// reported its own branch as merged, skipped the warning, and left the branch
/// behind after the worktree went. Risk must be judged from the main worktree.
#[test]
fn wt_rm_dot_sees_its_own_branch_as_unmerged() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    commit_in(&path, "new.txt", "unmerged work");

    let output = perch_args(&path, &["wt", "rm", "."]);

    assert!(
        !output.status.success(),
        "unmerged work should be flagged, not silently skipped; stderr: {}",
        stderr_str(&output)
    );
    assert!(
        stderr_str(&output).contains("unmerged"),
        "should name the unmerged commits; got: {}",
        stderr_str(&output)
    );
    assert!(path.exists(), "worktree must survive: {}", path.display());
}

/// `--force` on `.` must finish the job: no worktree *and* no leftover branch.
#[test]
fn wt_rm_dot_force_leaves_no_branch_behind() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    commit_in(&path, "new.txt", "unmerged work");

    let output = perch_args(&path, &["wt", "rm", ".", "--force"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    assert!(
        !path.exists(),
        "worktree should be gone: {}",
        path.display()
    );
    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        !stdout_str(&branches).lines().any(|l| l == "feature"),
        "no leftover branch; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn wt_rm_dot_in_the_main_worktree_errors() {
    let (_bare, parent, work) = setup_with_parent();
    // A removable worktree exists, so `.` fails on its own merits rather than
    // on there being nothing to remove at all.
    add_worktree(&work, &parent, "feature");

    let output = perch_args(&work, &["wt", "rm", "."]);

    assert!(
        !output.status.success(),
        "should exit non-zero; stderr: {}",
        stderr_str(&output)
    );
    assert!(
        stderr_str(&output).contains("main worktree cannot be removed"),
        "should explain why; got: {}",
        stderr_str(&output)
    );
}

#[test]
fn wt_rm_dot_refuses_dirty_worktree_non_interactively() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    fs::write(path.join("scratch.txt"), "uncommitted\n").unwrap();

    let output = perch_args(&path, &["wt", "rm", "."]);

    assert!(
        !output.status.success(),
        "should exit non-zero; stderr: {}",
        stderr_str(&output)
    );
    assert!(path.exists(), "worktree must survive: {}", path.display());
}

#[test]
fn double_dash_switches_to_branch_named_like_subcommand() {
    let (_bare, work) = setup();

    git(work.path(), &["branch", "wt"]);

    let output = perch_args(work.path(), &["--", "wt"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let head = git(work.path(), &["branch", "--show-current"]);
    assert_eq!(stdout_str(&head).trim(), "wt");

    // Switching off `main` makes it a stale merged branch, which triggers the
    // delete prompt. Non-interactively that must neither block nor act: `main`
    // must survive (regression guard for the multi_select TTY check).
    let branches = git(work.path(), &["branch", "--format=%(refname:short)"]);
    assert!(
        stdout_str(&branches).lines().any(|l| l == "main"),
        "main must not be auto-deleted in a non-interactive run; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn wt_rm_from_inside_doomed_worktree_hands_off_to_main() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "feature"]);
    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &["worktree", "add", path.to_str().unwrap(), "feature"],
    );

    // Run `wt rm feature` *from inside* the worktree being removed: cwd would
    // vanish, so it must chdir to main and hand that path off for the wrapper.
    let output = perch_args(&path, &["wt", "rm", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(!path.exists(), "worktree dir should be removed");

    let printed = stdout_str(&output).trim().to_string();
    assert!(
        Path::new(&printed).is_dir() && printed.ends_with("repo"),
        "stdout should be the main worktree path; got: {printed}"
    );
    assert!(
        poll_until(|| ready_trash(&path).is_none()),
        "the worker inherited the surviving main-worktree cwd and reclaimed the trash"
    );
}

#[test]
fn handoff_fast_forwards_held_worktree_from_its_own_remote() {
    let (_bare, parent, work) = setup_with_parent();

    let path = parent.path().join("worktrees").join("repo").join("feature");
    git(
        &work,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            path.to_str().unwrap(),
            "main",
        ],
    );

    // Publish a commit on `feature`, record it, then rewind the worktree so it
    // sits one commit behind its upstream.
    fs::write(path.join("f.txt"), "v1\n").unwrap();
    git(&path, &["add", "f.txt"]);
    git(&path, &["commit", "-m", "remote work"]);
    git(&path, &["push", "-u", "origin", "feature"]);
    let upstream = stdout_str(&git(&path, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    git(&path, &["reset", "--hard", "HEAD~1"]);

    // Plain `perch feature` from main: hands off and updates the worktree.
    let output = perch(&work, "feature");
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("Pulled 1 commit"),
        "should fast-forward the held worktree; stderr: {}",
        stderr_str(&output)
    );

    // The worktree (not the main checkout) must now be at the upstream commit.
    let head = stdout_str(&git(&path, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    assert_eq!(head, upstream, "worktree HEAD should be fast-forwarded");
}

#[test]
fn wt_rm_reports_failure_and_keeps_branch_when_worktree_is_locked() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");

    // A locked worktree survives even `--force` (git wants `--force --force`),
    // which we deliberately don't escalate to.
    git(&work, &["worktree", "lock", path.to_str().unwrap()]);

    let output = perch_args(&work, &["wt", "rm", "feature", "--force"]);
    // The command itself succeeds (per-worktree failures are reported, not fatal).
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        stderr_str(&output).contains("failed to remove"),
        "should report the removal failure; stderr: {}",
        stderr_str(&output)
    );

    // Nothing was destroyed: the worktree dir survives and the branch remains.
    assert!(path.exists(), "worktree dir should still exist on failure");
    let branches = git(&work, &["branch", "--format=%(refname:short)"]);
    assert!(
        stdout_str(&branches).lines().any(|l| l == "feature"),
        "branch must not be deleted when removal failed; got: {}",
        stdout_str(&branches)
    );
}

#[test]
fn wt_rm_from_inside_locked_worktree_hands_off_to_main() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    git(&work, &["worktree", "lock", path.to_str().unwrap()]);

    let output = perch_args(&path, &["wt", "rm", "feature", "--force"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let printed = stdout_str(&output).trim().to_string();
    assert!(
        Path::new(&printed).is_dir() && printed.ends_with("repo"),
        "stdout should be the main worktree path; got: {printed}"
    );
}

#[test]
fn wt_rm_clears_missing_detached_worktree_by_dir_name() {
    let (_bare, parent, work) = setup_with_parent();

    // A detached worktree whose directory was deleted by hand: it reports no
    // branch and lingers as a "prunable" registration. `wt rm` must still be
    // able to target it (by directory name) and clear the dead entry.
    let path = add_worktree_detached(&work, parent.path(), "scratch");
    fs::remove_dir_all(&path).unwrap();

    let output = perch_args(&work, &["wt", "rm", "scratch"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let list = stdout_str(&git(&work, &["worktree", "list", "--porcelain"]));
    assert!(
        !list.contains("prunable") && !list.contains("scratch"),
        "stale registration should be cleared; got: {list}"
    );
}

/// An `rm` on PATH that writes its pid to `started` once it runs and then blocks until
/// `gate` exists, so a test can observe the detached worker mid-unlink. The
/// guard opens the gate when the test ends, however it ends, so a failing
/// assertion never leaves the worker parked forever.
struct GatedRm {
    gate: PathBuf,
    path_env: OsString,
    started: PathBuf,
}

impl GatedRm {
    fn install(parent: &Path) -> Self {
        let bin = parent.join("bin");
        fs::create_dir(&bin).unwrap();
        let fake_rm = bin.join("rm");
        fs::write(
            &fake_rm,
            "#!/bin/sh\n\
             echo $$ > \"$PERCH_TEST_RM_STARTED\"\n\
             while [ ! -e \"$PERCH_TEST_RM_GATE\" ]; do sleep 0.01; done\n\
             exec /bin/rm \"$@\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_rm).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_rm, permissions).unwrap();
        let path_env = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        Self {
            gate: parent.join("allow-rm"),
            path_env,
            started: parent.join("rm-started"),
        }
    }

    fn open(&self) -> std::io::Result<()> {
        fs::write(&self.gate, "go\n")
    }

    /// The pid of the blocked `rm`, once it has started.
    fn rm_pid(&self) -> Option<libc::pid_t> {
        fs::read_to_string(&self.started).ok()?.trim().parse().ok()
    }
}

impl Drop for GatedRm {
    fn drop(&mut self) {
        let _ = self.open();
    }
}

/// The Ready trash a Removal left beside `worktree`, if it is still there.
fn ready_trash(worktree: &Path) -> Option<PathBuf> {
    fs::read_dir(worktree.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TRASH_PREFIX))
        })
}

fn reclamation_record_is_cleared(work: &Path) -> bool {
    Command::new("git")
        .args(["config", "--get-all", RECLAMATION_KEY])
        .current_dir(work)
        .output()
        .is_ok_and(|output| output.status.code() == Some(1))
}

#[test]
fn wt_rm_returns_while_the_detached_unlink_is_still_blocked() {
    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    let rm = GatedRm::install(parent.path());

    let output = perch_command(&work, &["wt", "rm", "feature"])
        .env("PERCH_NO_HOOKS", "1")
        .env("PERCH_TEST_RM_STARTED", &rm.started)
        .env("PERCH_TEST_RM_GATE", &rm.gate)
        .env("PATH", &rm.path_env)
        .output()
        .expect("failed to run perch");

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        poll_until(|| rm.started.exists()),
        "the detached deleter never reached rm"
    );
    assert!(!path.exists(), "the original path must already be absent");

    let trash = ready_trash(&path).expect("blocked unlink should leave the trash visible");
    assert!(trash.exists());

    let list = stdout_str(&git(&work, &["worktree", "list", "--porcelain"]));
    assert!(!list.contains("feature"), "registration survived: {list}");
    let branches = stdout_str(&git(&work, &["branch", "--format=%(refname:short)"]));
    assert!(
        !branches.lines().any(|branch| branch == "feature"),
        "branch survived: {branches}"
    );
    let record = stdout_str(&git(&work, &["config", "--get", RECLAMATION_KEY]));
    let duplicate = perch_command(&work, &[])
        .env("PERCH_INTERNAL_RECLAMATION", record.trim())
        .output()
        .expect("failed to run duplicate reclamation worker");
    assert!(
        duplicate.status.success(),
        "stderr: {}",
        stderr_str(&duplicate)
    );
    assert!(
        trash.exists(),
        "a duplicate worker reclaimed a directory already owned by a worker"
    );

    rm.open().unwrap();
    assert!(
        poll_until(|| !trash.exists()),
        "the staged directory survived after releasing rm"
    );
}

/// Every live pid whose session is `session`: the set a session manager
/// signals when it closes the pane that ran `perch`.
fn session_members(session: libc::pid_t) -> Vec<libc::pid_t> {
    let listing = Command::new("ps")
        .args(["-A", "-o", "pid="])
        .output()
        .expect("failed to list processes");
    stdout_str(&listing)
        .lines()
        .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
        // SAFETY: `getsid` only reads kernel state for the given pid.
        .filter(|&pid| unsafe { libc::getsid(pid) } == session)
        .collect()
}

#[test]
fn reclamation_survives_a_hangup_sent_to_the_invoking_session() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let (_bare, parent, work) = setup_with_parent();
    let path = add_worktree(&work, &parent, "feature");
    let rm = GatedRm::install(parent.path());

    // A pty child leads its own session, the way a pane shell does, so the
    // session it leaves behind can be swept the way a session manager does.
    let pty = native_pty_system()
        .openpty(PtySize::default())
        .expect("failed to open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_perch"));
    cmd.args(["wt", "rm", "feature"]);
    cmd.cwd(&work);
    cmd.env("PERCH_NO_HOOKS", "1");
    cmd.env("PERCH_TEST_RM_STARTED", &rm.started);
    cmd.env("PERCH_TEST_RM_GATE", &rm.gate);
    cmd.env("PATH", &rm.path_env);
    let mut child = ChildGuard(pty.slave.spawn_command(cmd).expect("failed to spawn"));
    drop(pty.slave);
    let session = libc::pid_t::try_from(child.0.process_id().expect("child has a pid")).unwrap();
    child.wait_bounded();
    let mut rm_pid = None;
    assert!(
        poll_until(|| {
            rm_pid = rm.rm_pid();
            rm_pid.is_some()
        }),
        "the detached deleter never reached rm"
    );
    let trash = ready_trash(&path).expect("blocked unlink should leave the trash visible");
    // The session check is the fast regression detector: it fires at once,
    // where the sweep below only fails once the trash poll times out. The
    // sweep is still what proves the behaviour the fix exists for.
    // SAFETY: `getsid` only reads kernel state for the given pid.
    let worker_session = unsafe { libc::getsid(rm_pid.unwrap()) };
    assert_ne!(
        worker_session, session,
        "the worker still belongs to the session that ran perch"
    );

    for pid in session_members(session) {
        // SAFETY: plain signal delivery to a pid this test's child created.
        unsafe { libc::kill(pid, libc::SIGHUP) };
    }
    rm.open().unwrap();

    assert!(
        poll_until(|| !trash.exists()),
        "hanging up the invoking session killed reclamation"
    );
    assert!(
        poll_until(|| reclamation_record_is_cleared(&work)),
        "successful reclamation should clear its durable record"
    );
}

/// Whether any live process was started with `helper` on its command line.
fn helper_is_running(helper: &Path) -> bool {
    let listing = Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .expect("failed to list processes");
    stdout_str(&listing).contains(helper.to_str().unwrap())
}

/// The fetch starts before the picker and must not outlive it: Esc and Ctrl-C
/// (a key in raw mode, not a signal, so nothing reaches the child on its own)
/// each leave no transport running once `perch` has exited, whether the
/// transport goes quietly on SIGTERM or has to be killed. And while the picker
/// is open the fetch has no terminal, so a transport that wants a passphrase
/// has nowhere to ask for one.
#[test]
fn dismissing_the_wt_picker_ends_a_background_fetch_that_never_reached_the_terminal() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::sync::Arc;

    // A transport that tries the terminal, says so, then hangs: the remote
    // helper protocol reads nothing back from it, so the fetch waits. The
    // second one also shrugs off SIGTERM, as a helper is free to.
    const HANGS: &str = "sleep 60";
    const HANGS_AND_TRAPS_TERM: &str = "trap '' TERM\nwhile :; do sleep 1; done";

    for (key, hang) in [
        (&b"\x1b"[..], HANGS),
        (&b"\x03"[..], HANGS),
        (&b"\x1b"[..], HANGS_AND_TRAPS_TERM),
        (&b"\x03"[..], HANGS_AND_TRAPS_TERM),
    ] {
        let (_bare, parent, work) = setup_with_parent();
        let tried = parent.path().join("tried-the-terminal");
        let helper = parent.path().join("hang.sh");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf 'PASSPHRASE?' > /dev/tty\ntouch '{}'\n{hang}\n",
                tried.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &work,
            &[
                "remote",
                "set-url",
                "origin",
                &format!("ext::{}", helper.display()),
            ],
        );
        git(&work, &["config", "protocol.ext.allow", "always"]);

        let pty = native_pty_system()
            .openpty(PtySize::default())
            .expect("failed to open pty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_perch"));
        cmd.arg("wt");
        cmd.cwd(&work);
        cmd.env("PERCH_NO_HOOKS", "1");
        let mut child = ChildGuard(pty.slave.spawn_command(cmd).expect("failed to spawn"));
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&seen);
        let output = std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                collected.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });

        assert!(poll_until(|| tried.exists()), "the transport never ran");
        wait_for(&seen, "(type to filter):");
        writer.write_all(key).unwrap();
        writer.flush().unwrap();

        child.wait_bounded();
        drop(writer);
        drop(pty.master);
        output.join().unwrap();

        let screen = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
        assert!(
            !screen.contains("PASSPHRASE?"),
            "the background fetch reached the terminal: {screen}"
        );
        assert!(
            poll_until(|| !helper_is_running(&helper)),
            "the fetch outlived perch after {key:?} with a helper that does `{hang}`"
        );
    }
}

/// An askpass program prompts without a terminal, so from the background
/// fetch it would open a dialog behind the picker, or block it. Only the
/// foreground retry, where the user is waiting on the fetch, may run one —
/// whether git asks for a credential or ssh for a passphrase. A credential
/// helper can open a window of its own just the same, so the background fetch
/// asks it not to.
#[test]
fn only_the_foreground_retry_prompts() {
    enum Prompt {
        Askpass,
        CredentialHelper,
        SshPassphrase,
    }

    for prompt in [
        Prompt::Askpass,
        Prompt::CredentialHelper,
        Prompt::SshPassphrase,
    ] {
        let (_bare, parent, work) = setup_with_parent();
        let asked = parent.path().join("asked");
        let prompter = parent.path().join("prompter.sh");
        // Records each prompt shown. The background fetch runs with
        // `GIT_TERMINAL_PROMPT=0`, the retry without. As a credential helper
        // it listens to `credential.interactive`, as Git Credential Manager
        // does, and answers only `get`.
        fs::write(
            &prompter,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in store|erase) exit 0 ;; esac\n\
                 [ \"$(git config --get credential.interactive)\" = false ] && exit 0\n\
                 echo \"prompt=${{GIT_TERMINAL_PROMPT:-unset}}\" >> '{}'\n\
                 case \"$1\" in get) echo username=x; echo password=x ;; *) echo x ;; esac\n",
                asked.display()
            ),
        )
        .unwrap();
        // A transport that wants credentials or a passphrase, then fails
        // either way. The ssh one, like an OpenSSH too old for
        // `SSH_ASKPASS_REQUIRE`, runs whatever `SSH_ASKPASS` names when it has
        // no terminal.
        let helper = match prompt {
            Prompt::Askpass | Prompt::SshPassphrase => String::new(),
            Prompt::CredentialHelper => format!("!{}", prompter.display()),
        };
        let script = match prompt {
            Prompt::Askpass | Prompt::CredentialHelper => format!(
                "#!/bin/sh\nprintf 'protocol=https\\nhost=example.com\\n\\n' \\\n  \
                 | git -c credential.helper= -c credential.helper='{helper}' credential fill >/dev/null\n\
                 exit 1\n"
            ),
            Prompt::SshPassphrase => {
                "#!/bin/sh\n[ -n \"$SSH_ASKPASS\" ] && \"$SSH_ASKPASS\" 'Passphrase:' >/dev/null\nexit 1\n"
                    .to_string()
            }
        };
        let transport = parent.path().join("transport.sh");
        fs::write(&transport, script).unwrap();
        for script in [&prompter, &transport] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        match prompt {
            Prompt::Askpass | Prompt::CredentialHelper => {
                git(
                    &work,
                    &[
                        "remote",
                        "set-url",
                        "origin",
                        &format!("ext::{}", transport.display()),
                    ],
                );
                git(&work, &["config", "protocol.ext.allow", "always"]);
            }
            Prompt::SshPassphrase => {
                git(
                    &work,
                    &[
                        "remote",
                        "set-url",
                        "origin",
                        "ssh://example.invalid/repo.git",
                    ],
                );
                git(
                    &work,
                    &["config", "core.sshCommand", transport.to_str().unwrap()],
                );
                git(&work, &["config", "ssh.variant", "simple"]);
            }
        }
        let mut command = perch_command(&work, &["wt", "feature", "--no-switch"]);
        command.env("PERCH_NO_HOOKS", "1");
        // The helper route must reach the helper, not an askpass.
        if !matches!(prompt, Prompt::CredentialHelper) {
            command
                .env("GIT_ASKPASS", &prompter)
                .env("SSH_ASKPASS", &prompter);
        }

        command.output().expect("failed to run perch");

        let asked = fs::read_to_string(&asked).unwrap_or_default();
        let route = match prompt {
            Prompt::Askpass => "askpass",
            Prompt::CredentialHelper => "credential helper",
            Prompt::SshPassphrase => "ssh askpass",
        };
        assert!(
            !asked.is_empty(),
            "{route}: the foreground retry never prompted"
        );
        assert!(
            !asked.contains("prompt=0"),
            "{route}: the background fetch prompted: {asked}"
        );
    }
}

/// A real SIGINT leaves through the handler in `main`, which exits without
/// unwinding — so the drop guard never runs, and the fetch leads a session of
/// its own that the signal never reached. Nothing else would end it.
#[test]
fn a_real_sigint_ends_the_background_fetch_before_perch_exits() {
    let (_bare, parent, work) = setup_with_parent();
    let tried = parent.path().join("tried-the-transport");
    let helper = parent.path().join("hang.sh");
    // The transport shrugs off SIGTERM, so ending it takes the whole grace —
    // the window in which a run that did not stop at the interrupt would get
    // as far as making the worktree.
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\ntouch '{}'\ntrap '' TERM\nwhile :; do sleep 1; done\n",
            tried.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("ext::{}", helper.display()),
        ],
    );
    git(&work, &["config", "protocol.ext.allow", "always"]);

    // A named target needs no terminal, so the run reaches the join — and
    // blocks there on the fetch — with no picker in the way.
    let mut child = perch_command(&work, &["wt", "feature", "--no-switch"])
        .env("PERCH_NO_HOOKS", "1")
        .spawn()
        .expect("failed to spawn perch");
    assert!(poll_until(|| tried.exists()), "the transport never ran");

    let pid = libc::pid_t::try_from(child.id()).unwrap();
    // SAFETY: plain signal delivery to a child this test spawned.
    unsafe { libc::kill(pid, libc::SIGINT) };
    let status = child.wait().expect("failed to wait for perch");

    assert_eq!(status.code(), Some(130), "perch did not exit on the signal");
    assert!(
        poll_until(|| !helper_is_running(&helper)),
        "the fetch outlived perch after a real SIGINT"
    );
    // The interrupt asked for none of the work the run was about to do, and
    // laying out the directory a worktree goes in is the first of it.
    let worktrees = parent.path().join("worktrees");
    assert!(
        !worktrees.exists(),
        "the interrupted run carried on into making a worktree at {}",
        worktrees.display()
    );
}

#[test]
fn the_next_wt_command_retries_an_exact_recorded_external_trash_path() {
    let (_bare, parent, work) = setup_with_parent();
    let manual_parent = parent.path().join("manual-worktrees");
    fs::create_dir(&manual_parent).unwrap();
    let trash = manual_parent.join(format!("{TRASH_PREFIX}manual.123"));
    let original = manual_parent.join("manual");
    fs::create_dir(&trash).unwrap();
    fs::write(trash.join("leftover"), "content\n").unwrap();
    let record = reclamation_record("ready", &original, &trash);
    git(&work, &["config", "--add", RECLAMATION_KEY, &record]);

    let output = perch_args(&work, &["wt", "ls"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        poll_until(|| !trash.exists()),
        "the next wt command did not retry the recorded path"
    );
    assert!(
        poll_until(|| reclamation_record_is_cleared(&work)),
        "successful reclamation should clear its durable record"
    );
}

#[test]
fn a_staged_record_deregisters_before_reclaiming_after_a_crash() {
    let (_bare, parent, work) = setup_with_parent();
    let original = add_worktree(&work, &parent, "feature");
    let trash = original
        .parent()
        .unwrap()
        .join(format!("{TRASH_PREFIX}feature.crashed"));
    fs::rename(&original, &trash).unwrap();
    let record = reclamation_record("staged", &original, &trash);
    git(&work, &["config", "--add", RECLAMATION_KEY, &record]);

    let output = perch_args(&work, &["wt", "ls"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        poll_until(|| !trash.exists()),
        "staged crash recovery did not reclaim the directory"
    );
    assert!(
        poll_until(|| {
            !stdout_str(&git(&work, &["worktree", "list", "--porcelain"]))
                .contains(original.to_str().unwrap())
        }),
        "reclamation ran before the missing registration was cleared"
    );
}

#[test]
fn a_staged_record_never_reclaims_an_existing_original() {
    let (_bare, parent, work) = setup_with_parent();
    let original = parent.path().join("manual-worktree");
    let trash = parent
        .path()
        .join(format!("{TRASH_PREFIX}manual-worktree.crashed"));
    fs::create_dir(&original).unwrap();
    fs::write(original.join("keep"), "content\n").unwrap();
    let record = reclamation_record("staged", &original, &trash);
    git(&work, &["config", "--add", RECLAMATION_KEY, &record]);

    let output = perch_args(&work, &["wt", "ls"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(original.join("keep").exists());
    assert!(
        poll_until(|| reclamation_record_is_cleared(&work)),
        "a restored staged record should be cleared"
    );
}

/// How long the pty test is willing to wait on its child. Generous enough that
/// a loaded CI host redrawing a pty is never mistaken for a hang, but finite so
/// a child that wedges fails the test instead of stalling the whole run.
const PATIENCE: Duration = Duration::from_secs(30);
/// Gap between polls: short enough to add no perceptible delay to a run that
/// behaves, long enough not to spin a core while waiting.
const POLL: Duration = Duration::from_millis(10);

/// Waits for `ready` to hold, reporting whether it did before `PATIENCE` ran
/// out. Polling what the child has actually done keeps the pty test off a fixed
/// sleep, so a slow CI host waits longer instead of racing ahead.
fn poll_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Kills the child when the test ends, however it ends. An assertion that fires
/// mid-session — a `wait_for` timeout, say — unwinds with the picker still on
/// screen, and an orphaned pty-attached `perch` would outlive the test.
struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl ChildGuard {
    /// Blocks until the child exits, giving up after `PATIENCE`. Polling rather
    /// than a plain `wait()` keeps a child that never exits from hanging the
    /// test binary indefinitely.
    fn wait_bounded(&mut self) {
        let exited = poll_until(|| self.0.try_wait().expect("failed to poll child").is_some());
        assert!(exited, "child did not exit within {PATIENCE:?}");
    }
}

/// Blocks until the child has written `needle` to the pty, so keys are only
/// sent once the prompt they answer is on screen.
fn wait_for(seen: &Mutex<Vec<u8>>, needle: &str) {
    let drawn = poll_until(|| {
        let buf = seen.lock().unwrap();
        buf.windows(needle.len()).any(|w| w == needle.as_bytes())
    });
    assert!(
        drawn,
        "timed out waiting for {needle:?}; got: {}",
        String::from_utf8_lossy(&seen.lock().unwrap())
    );
}

/// Drives the post-switch cleanup prompt over a real pty — the only way to see
/// the rows it draws and the deletions it performs, since a piped run declines
/// to prompt at all. Waits for `row` to be drawn, ticks every row with `→`, runs
/// `before_confirm`, confirms, and returns every byte the child wrote.
///
/// `hooks` leaves worktree hooks on for the tests that configure one in the repo
/// under test; every other caller wants them off, as [`perch_args`] does.
fn drive_multi_select_prompt(
    work: &Path,
    args: &[&str],
    row: &str,
    hooks: bool,
    before_confirm: impl FnOnce(),
) -> Vec<u8> {
    drive_multi_select_prompt_until(
        work,
        args,
        row,
        hooks,
        before_confirm,
        MultiSelectFinish::Accept,
    )
}

fn drive_multi_select_prompt_then_escape(
    work: &Path,
    args: &[&str],
    row: &str,
    prompt: &str,
) -> Vec<u8> {
    drive_multi_select_prompt_until(
        work,
        args,
        row,
        false,
        || {},
        MultiSelectFinish::EscapeAt(prompt),
    )
}

#[derive(Clone, Copy)]
enum MultiSelectFinish<'a> {
    Accept,
    EscapeAt(&'a str),
}

fn drive_multi_select_prompt_until(
    work: &Path,
    args: &[&str],
    row: &str,
    hooks: bool,
    before_confirm: impl FnOnce(),
    finish: MultiSelectFinish<'_>,
) -> Vec<u8> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::sync::Arc;

    let pty = native_pty_system()
        .openpty(PtySize::default())
        .expect("failed to open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_perch"));
    cmd.args(args);
    cmd.cwd(work);
    if !hooks {
        cmd.env("PERCH_NO_HOOKS", "1");
    }
    let mut child = ChildGuard(pty.slave.spawn_command(cmd).expect("failed to spawn"));
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    // Read on a thread into a buffer the test can watch: the pty must keep
    // draining or the child blocks on a full buffer while we wait to send keys.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&seen);
    let output = std::thread::spawn(move || {
        // The loop reassembles the stream whatever the chunk size, so 1 KiB is
        // simply enough to swallow a picker redraw in a read or two.
        let mut chunk = [0u8; 1024];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            collected.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });

    // Drive the picker off what it has drawn rather than off a clock: `→` ticks
    // every row, Enter confirms, and each key waits for the redraw that proves
    // the last one landed.
    wait_for(&seen, &format!("[ ] {row}"));
    writer.write_all(b"\x1b[C").unwrap();
    writer.flush().unwrap();
    wait_for(&seen, &format!("[x] {row}"));
    before_confirm();
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    if let MultiSelectFinish::EscapeAt(prompt) = finish {
        wait_for(&seen, prompt);
        writer.write_all(b"\x1b").unwrap();
        writer.flush().unwrap();
    }

    child.wait_bounded();
    drop(writer);
    drop(pty.master);
    output.join().unwrap();
    Arc::try_unwrap(seen).unwrap().into_inner().unwrap()
}

fn drive_cleanup_prompt(
    work: &Path,
    target: &str,
    row: &str,
    hooks: bool,
    before_confirm: impl FnOnce(),
) -> Vec<u8> {
    drive_multi_select_prompt(work, &[target], row, hooks, before_confirm)
}

fn drive_enter_confirmation(work: &Path, args: &[&str], prompt: &str) -> String {
    drive_confirmation(work, args, prompt, b"\r")
}

fn drive_escape_confirmation(work: &Path, args: &[&str], prompt: &str) -> String {
    drive_confirmation(work, args, prompt, b"\x1b")
}

fn drive_confirmation(work: &Path, args: &[&str], prompt: &str, key: &[u8]) -> String {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::sync::Arc;

    let pty = native_pty_system()
        .openpty(PtySize::default())
        .expect("failed to open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_perch"));
    cmd.args(args);
    cmd.cwd(work);
    cmd.env("PERCH_NO_HOOKS", "1");
    let mut child = ChildGuard(pty.slave.spawn_command(cmd).expect("failed to spawn"));
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&seen);
    let output = std::thread::spawn(move || {
        let mut chunk = [0u8; 1024];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            collected.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });

    wait_for(&seen, prompt);
    writer.write_all(key).unwrap();
    writer.flush().unwrap();

    child.wait_bounded();
    drop(writer);
    drop(pty.master);
    output.join().unwrap();
    String::from_utf8_lossy(&Arc::try_unwrap(seen).unwrap().into_inner().unwrap()).into_owned()
}

/// [`drive_cleanup_prompt`] with hooks off and nothing to do between ticking and
/// confirming, read back as text — what most callers want.
fn cleanup_prompt(work: &Path, target: &str, row: &str) -> String {
    String::from_utf8_lossy(&drive_cleanup_prompt(work, target, row, false, || {})).into_owned()
}

/// Drives the branch picker over a real pty: filters to `branch`, waits for the
/// row to be drawn, runs `mid_prompt` while the picker is still waiting on a
/// keystroke, then selects. Returns every byte the child wrote.
///
/// Separate from [`drive_cleanup_prompt`], which drives the multi-select that
/// comes *after* a switch — this one drives the single-select that chooses it.
fn drive_branch_picker(
    work: &Path,
    verb: Option<&str>,
    branch: &str,
    mid_prompt: impl FnOnce(),
) -> Vec<u8> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::sync::Arc;

    let pty = native_pty_system()
        .openpty(PtySize::default())
        .expect("failed to open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_perch"));
    if let Some(verb) = verb {
        cmd.arg(verb);
    }
    cmd.cwd(work);
    cmd.env("PERCH_NO_HOOKS", "1");
    let mut child = ChildGuard(pty.slave.spawn_command(cmd).expect("failed to spawn"));
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&seen);
    let output = std::thread::spawn(move || {
        let mut chunk = [0u8; 1024];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            collected.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });

    // Filter to the branch, then wait for the cursor to be drawn on it — that
    // redraw is what proves the keys landed, and it has to happen before the
    // repo is disturbed, since the point is to move while the picker waits.
    // Matching the row rather than the echoed filter keeps the needle clear of
    // the styling around the prompt.
    wait_for(&seen, branch);
    writer.write_all(branch.as_bytes()).unwrap();
    writer.flush().unwrap();
    wait_for(&seen, &format!(">   {branch}"));

    mid_prompt();

    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();

    child.wait_bounded();
    drop(writer);
    drop(pty.master);
    output.join().unwrap();
    Arc::try_unwrap(seen).unwrap().into_inner().unwrap()
}

/// The picker's list is a snapshot, but the hand-off decision must not be one:
/// the picker sits waiting on a keystroke, and a worktree taken on the target in
/// that window makes the checkout this would otherwise attempt illegal. Git
/// refuses it outright, so a stale snapshot turns a hand-off into an error.
#[test]
fn a_worktree_taken_while_the_picker_is_open_is_handed_off_to() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "target"]);
    let held = parent.path().join("held");

    let raw = drive_branch_picker(&work, None, "target", || {
        git(
            &work,
            &["worktree", "add", held.to_str().unwrap(), "target"],
        );
    });
    let text = String::from_utf8_lossy(&raw);

    assert!(
        text.contains("is checked out at"),
        "should hand off to the worktree taken mid-prompt; got: {text}"
    );
    assert!(
        !text.contains("already used by worktree"),
        "and must not attempt the checkout git forbids; got: {text}"
    );
}

/// The same window, in the verb that builds worktrees rather than entering
/// them: `wt` would otherwise take the branch for one that still needs a
/// worktree and run `git worktree add` over the one that now exists.
#[test]
fn a_worktree_taken_while_the_wt_picker_is_open_is_entered_not_rebuilt() {
    let (_bare, parent, work) = setup_with_parent();

    git(&work, &["branch", "target"]);
    let held = parent.path().join("held");

    let raw = drive_branch_picker(&work, Some("wt"), "target", || {
        git(
            &work,
            &["worktree", "add", held.to_str().unwrap(), "target"],
        );
    });
    let text = String::from_utf8_lossy(&raw);

    assert!(
        text.contains("switched to worktree at"),
        "should enter the worktree taken mid-prompt; got: {text}"
    );
    assert!(
        !text.contains("already used by worktree"),
        "and must not try to build a second one; got: {text}"
    );
}

/// The same window again, but with the branch going away rather than gaining a
/// worktree. Re-reading state answers what the branch needs; it cannot answer
/// what the user asked for, and a picked row that no longer resolves is not a
/// request to create the name afresh from the default branch.
#[test]
fn a_branch_deleted_while_the_wt_picker_is_open_is_reported_not_recreated() {
    let (_bare, _parent, work) = setup_with_parent();

    git(&work, &["branch", "target"]);

    let raw = drive_branch_picker(&work, Some("wt"), "target", || {
        git(&work, &["branch", "-D", "target"]);
    });
    let text = String::from_utf8_lossy(&raw);

    assert!(
        text.contains("no longer exists"),
        "should report the branch that went away; got: {text}"
    );
    assert!(
        !text.contains("created"),
        "and must not create a fresh branch in its place; got: {text}"
    );
}

/// The stale-branch picker holds the terminal in raw mode, where a bare `\n`
/// drops a line without returning to column 0. Printing the deletion outcomes
/// before that mode is released staircases them across the screen, so this
/// drives the picker over a real pty and insists every newline is a CRLF.
#[test]
fn stale_deletion_outcomes_are_not_printed_in_raw_mode() {
    let (_bare, work) = setup();

    // Three stale branches — each published, then its upstream deleted — plus a
    // destination to switch to, so the post-switch cleanup prompt fires.
    for branch in ["aaa", "bbb", "ccc"] {
        git(work.path(), &["checkout", "-b", branch]);
        git(work.path(), &["push", "-u", "origin", branch]);
        git(work.path(), &["push", "origin", "--delete", branch]);
    }
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let raw = drive_cleanup_prompt(work.path(), "dest", "ccc", false, || {});
    let text = String::from_utf8_lossy(&raw);

    let deletions = text.matches(" deleted ").count();
    assert_eq!(
        deletions, 3,
        "expected all three ticked branches to report a deletion; got: {text}"
    );

    let staircased = raw
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' && (i == 0 || raw[i - 1] != b'\r'))
        .count();
    assert_eq!(
        staircased, 0,
        "every newline written to a tty must be CRLF; got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Equivalence
// ---------------------------------------------------------------------------

/// A branch with a commit of its own, published under its own name — what a
/// topic branch looks like the moment before it is merged on the forge.
fn push_topic_branch(work: &Path, branch: &str) {
    git(work, &["checkout", "-b", branch]);
    commit_in(work, &format!("{branch}.txt"), "topic work");
    git(work, &["push", "-u", "origin", branch]);
}

/// Land `branch`'s work on the remote the way a forge squash-merge does: one new
/// commit on `main` carrying the whole diff under a hash of its own, then the
/// branch's upstream deleted. What is left locally is a branch that is stale on
/// the *Gone* ground and unmerged by every test git offers.
fn squash_merge_upstream(work: &Path, branch: &str) {
    git(work, &["checkout", "main"]);
    git(work, &["merge", "--squash", branch]);
    git(work, &["commit", "-m", &format!("squash {branch}")]);
    git(work, &["push", "origin", "main"]);
    git(work, &["push", "origin", "--delete", branch]);
    git(work, &["fetch", "--prune", "origin"]);
}

/// Land `branch`'s two commits on the remote the way a forge *rebase*-merge
/// does: each replayed onto `main` under a hash of its own, so no single commit
/// there carries the branch's whole diff. Then the upstream is deleted, as after
/// a squash merge.
///
/// `-x` is what makes it a replay rather than a fast-forward: it rewords each
/// commit, so the ones landing on `main` are new objects. Without it git may
/// produce byte-identical commits, which share the branch's hashes and move the
/// merge-base — a different scenario entirely, and one this test isn't about.
fn rebase_merge_upstream(work: &Path, branch: &str) {
    git(work, &["checkout", "main"]);
    git(work, &["cherry-pick", "-x", &format!("{branch}~1"), branch]);
    git(work, &["push", "origin", "main"]);
    git(work, &["push", "origin", "--delete", branch]);
    git(work, &["fetch", "--prune", "origin"]);
}

/// The local branch names, as one blob to search — enough to answer "did this
/// branch survive?".
fn branch_listing(work: &Path) -> String {
    stdout_str(&git(work, &["branch", "--format=%(refname:short)"]))
}

/// The whole point of *Equivalent*: a branch whose diff the anchor already
/// holds under another hash warns of nothing, so it draws no marker, earns no
/// legend, and goes — even though `git branch -d` would refuse it.
#[test]
fn a_squash_merged_branch_is_deleted_without_a_warning() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "feature");
    squash_merge_upstream(work.path(), "feature");
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        !text.contains('↑'),
        "a proven branch destroys nothing, so no marker: {text}"
    );
    assert!(
        !text.contains("unmerged commits"),
        "and nothing for a legend to gloss: {text}"
    );
    assert!(
        !branch_listing(work.path()).contains("feature"),
        "the branch should be gone: {}",
        branch_listing(work.path())
    );
}

/// The proof is about content, and a commit on top is content the anchor has
/// never seen. Landing the rest of the branch buys it nothing: the warning —
/// and the license it carries — stand.
#[test]
fn a_commit_on_top_of_a_squash_merge_keeps_its_warning() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "feature");
    squash_merge_upstream(work.path(), "feature");
    git(work.path(), &["checkout", "feature"]);
    commit_in(work.path(), "later.txt", "work done after the merge");
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        text.contains('↑'),
        "unique work is still at risk, so the marker stands: {text}"
    );
    assert!(
        text.contains("unmerged commits"),
        "and the legend still glosses it: {text}"
    );
    // ADR 0001 from there on: the marker was shown, so ticking the row discards
    // the commits it warned about. Equivalence changed nothing here.
    assert!(
        !branch_listing(work.path()).contains("feature"),
        "a warned row is still deleted when ticked: {}",
        branch_listing(work.path())
    );
}

/// A rebase-merge replays each commit separately, so no commit on the anchor
/// carries the branch's whole diff and the patch-id route finds nothing. The
/// content route answers it: the files the branch touched read identically on
/// the anchor, however they got there.
#[test]
fn a_rebase_merged_branch_is_deleted_without_a_warning() {
    let (_bare, work) = setup();

    git(work.path(), &["checkout", "-b", "feature"]);
    commit_in(work.path(), "one.txt", "first");
    commit_in(work.path(), "two.txt", "second");
    git(work.path(), &["push", "-u", "origin", "feature"]);
    rebase_merge_upstream(work.path(), "feature");
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        !text.contains('↑'),
        "replayed commit by commit is still landed: {text}"
    );
    assert!(
        !branch_listing(work.path()).contains("feature"),
        "the branch should be gone: {}",
        branch_listing(work.path())
    );
}

/// `git cherry` compares patch ids, which are normalised: they ignore
/// whitespace, so a branch differing from what landed by whitespace alone would
/// pass. That is fine for `git rebase`, which leaves the branch behind; it is
/// not fine for a force-delete, so the match is confirmed verbatim.
#[test]
fn a_branch_differing_only_in_whitespace_is_not_proven() {
    let (_bare, work) = setup();

    fs::write(work.path().join("a.txt"), "foo\n").unwrap();
    git(work.path(), &["add", "."]);
    git(
        work.path(),
        &["commit", "-m", "the line before either edit"],
    );
    git(work.path(), &["push", "origin", "main"]);

    git(work.path(), &["checkout", "-b", "feature"]);
    fs::write(work.path().join("a.txt"), "foo bar\n").unwrap();
    git(work.path(), &["commit", "-am", "spaced"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    // What landed says `foobar`, not `foo bar` — the same patch to git's
    // normalised reckoning, a different file to anyone reading it.
    git(work.path(), &["checkout", "main"]);
    fs::write(work.path().join("a.txt"), "foobar\n").unwrap();
    git(work.path(), &["commit", "-am", "unspaced"]);
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["push", "origin", "--delete", "feature"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        text.contains('↑'),
        "the whitespace is the branch's own unique work: {text}"
    );
}

/// The point of keeping the patch route at all: it answers a branch whose work
/// landed even after the anchor has moved on over the same file. Confirming the
/// match verbatim must not cost that — patch ids ignore line numbers, so a later
/// edit shifting every hunk header leaves the proof standing.
#[test]
fn a_squash_merged_branch_is_still_proven_once_the_anchor_moves_on() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "feature");
    squash_merge_upstream(work.path(), "feature");
    // An unrelated edit to the same file, above the branch's own change.
    let landed = fs::read_to_string(work.path().join("feature.txt")).unwrap();
    fs::write(
        work.path().join("feature.txt"),
        format!("a line added later\n{landed}"),
    )
    .unwrap();
    git(work.path(), &["commit", "-am", "later work above it"]);
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        !text.contains('↑'),
        "the branch's patch is still on the anchor, wherever it now sits: {text}"
    );
    assert!(
        !branch_listing(work.path()).contains("feature"),
        "the branch should be gone: {}",
        branch_listing(work.path())
    );
}

/// The content route compares the paths a branch touched, and git reports a
/// rename as its destination alone — which would leave the deletion of its
/// source uncompared, and prove a branch whose deletion never landed.
#[test]
fn a_rename_whose_deletion_never_landed_is_not_proven() {
    let (_bare, work) = setup();

    // The file predates the branch, so removing it is work of the branch's own.
    commit_in(work.path(), "old.txt", "a file to be renamed");
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["checkout", "-b", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    git(work.path(), &["mv", "old.txt", "new.txt"]);
    git(work.path(), &["commit", "-m", "rename it"]);

    // Only the arrival lands on main; `old.txt` stays, so the branch still holds
    // a deletion the anchor has never seen.
    git(work.path(), &["checkout", "main"]);
    let moved = stdout_str(&git(work.path(), &["show", "feature:new.txt"]));
    fs::write(work.path().join("new.txt"), moved).unwrap();
    git(work.path(), &["add", "new.txt"]);
    git(work.path(), &["commit", "-m", "add the new name only"]);
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["push", "origin", "--delete", "feature"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        text.contains('↑'),
        "the deletion is unique work, so the warning stands: {text}"
    );
}

/// A license covers the commit it was established at. Move the branch after the
/// proof and the delete falls back to `git branch -d`, which refuses it — the
/// same guard an unmarked worktree meets.
#[test]
fn a_branch_that_moves_after_the_proof_is_no_longer_covered_by_it() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "feature");
    squash_merge_upstream(work.path(), "feature");
    // A commit for the branch to be moved onto, parked out of the way on a
    // branch of its own so nothing else notices it.
    git(work.path(), &["checkout", "-b", "parked"]);
    commit_in(work.path(), "later.txt", "work done after the proof");
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["branch", "dest", "main"]);

    // The rows — and the proof — are built before the picker draws, so moving
    // the branch now is exactly the race the pin exists for.
    let raw = drive_cleanup_prompt(work.path(), "dest", "feature", false, || {
        git(work.path(), &["branch", "--force", "feature", "parked"]);
    });
    let text = String::from_utf8_lossy(&raw);

    assert!(
        branch_listing(work.path()).contains("feature"),
        "the proof no longer covers where the branch points, so git refuses: {text}"
    );
}

/// The content route reads a diff, and repository configuration can shrink one:
/// `diff.ignoreSubmodules=all` hides a changed gitlink from both the path list
/// and the comparison, so a branch carrying a landed file edit plus a submodule
/// bump of its own would be proven on the half git chose to show.
#[test]
fn configuration_cannot_shrink_the_diff_a_proof_reads() {
    let (_bare, work) = setup();

    commit_in(work.path(), "a.txt", "the line before either edit");
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["config", "diff.ignoreSubmodules", "all"]);

    // A landed edit and a submodule bump that never landed, in one branch.
    git(work.path(), &["checkout", "-b", "feature"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);
    fs::write(work.path().join("a.txt"), "edited\n").unwrap();
    git(work.path(), &["add", "a.txt"]);
    let gitlink = stdout_str(&git(work.path(), &["rev-parse", "HEAD"]));
    git(
        work.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},sub", gitlink.trim()),
        ],
    );
    git(
        work.path(),
        &["commit", "-m", "edit a file, bump a submodule"],
    );

    // Only the file edit lands.
    git(work.path(), &["checkout", "main"]);
    fs::write(work.path().join("a.txt"), "edited\n").unwrap();
    git(work.path(), &["commit", "-am", "the file edit alone"]);
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["push", "origin", "--delete", "feature"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        text.contains('↑'),
        "the submodule bump is unique work, whatever the config shows: {text}"
    );
}

/// A deleted branch must not leave its upstream config behind for a later branch
/// of the same name to inherit — and the section-wide removal git offers cannot
/// parse every name git itself allows.
#[test]
fn a_proven_delete_clears_config_for_an_awkward_branch_name() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "topic]x");
    squash_merge_upstream(work.path(), "topic]x");
    git(work.path(), &["branch", "dest", "main"]);
    assert!(
        stdout_str(&git(work.path(), &["config", "--list", "--name-only"]))
            .contains("branch.topic]x."),
        "precondition: the branch should have tracking config to leave behind"
    );

    cleanup_prompt(work.path(), "dest", "topic]x");

    assert!(
        !branch_listing(work.path()).contains("topic]x"),
        "the branch should be gone"
    );
    let config = stdout_str(&git(work.path(), &["config", "--list", "--name-only"]));
    assert!(
        !config.contains("branch.topic]x."),
        "its config should have gone with it; got: {config}"
    );
}

/// The exact comparison is only exact if it reads the file rather than what a
/// driver makes of it: a textconv that strips spaces renders `foo bar` and
/// `foobar` alike, which would agree with the normalised patch ids that let the
/// whitespace case through in the first place.
#[test]
fn a_textconv_driver_cannot_make_the_exact_comparison_agree() {
    let (_bare, work) = setup();

    fs::write(work.path().join("a.txt"), "foo\n").unwrap();
    fs::write(work.path().join(".gitattributes"), "*.txt diff=nospace\n").unwrap();
    git(work.path(), &["add", "."]);
    git(
        work.path(),
        &["commit", "-m", "the line before either edit"],
    );
    // A driver that renders every version with its spaces removed.
    git(
        work.path(),
        &["config", "diff.nospace.textconv", "tr -d ' ' <"],
    );
    git(work.path(), &["push", "origin", "main"]);

    git(work.path(), &["checkout", "-b", "feature"]);
    fs::write(work.path().join("a.txt"), "foo bar\n").unwrap();
    git(work.path(), &["commit", "-am", "spaced"]);
    git(work.path(), &["push", "-u", "origin", "feature"]);

    git(work.path(), &["checkout", "main"]);
    fs::write(work.path().join("a.txt"), "foobar\n").unwrap();
    git(work.path(), &["commit", "-am", "unspaced"]);
    git(work.path(), &["push", "origin", "main"]);
    git(work.path(), &["push", "origin", "--delete", "feature"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        text.contains('↑'),
        "the spaces are the branch's own, whatever the driver renders: {text}"
    );
}

/// `git config --list` repeats a multi-valued key, and the `--unset-all` that
/// clears the first mention leaves the second with nothing to do. Reading exit
/// codes would call that a leftover and warn about config that is long gone.
#[test]
fn a_multi_valued_config_key_is_not_reported_as_left_behind() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "feature");
    squash_merge_upstream(work.path(), "feature");
    git(
        work.path(),
        &[
            "config",
            "--add",
            "branch.feature.merge",
            "refs/heads/other",
        ],
    );
    git(work.path(), &["branch", "dest", "main"]);

    let text = cleanup_prompt(work.path(), "dest", "feature");

    assert!(
        !text.contains("config"),
        "both values went, so there is nothing to warn about: {text}"
    );
    let config = stdout_str(&git(work.path(), &["config", "--list", "--name-only"]));
    assert!(
        !config.contains("branch.feature."),
        "and both really did go; got: {config}"
    );
}

/// A branch name may contain dots, and git reads a config name by its first and
/// last dot alone — so `branch.topic.extra.remote` belongs to `topic.extra`.
/// Deleting `topic` must not clear it.
#[test]
fn a_proven_delete_leaves_a_neighbouring_branchs_config_alone() {
    let (_bare, work) = setup();

    push_topic_branch(work.path(), "topic");
    squash_merge_upstream(work.path(), "topic");
    git(work.path(), &["branch", "topic.extra", "main"]);
    git(
        work.path(),
        &["config", "branch.topic.extra.remote", "origin"],
    );
    git(work.path(), &["branch", "dest", "main"]);

    cleanup_prompt(work.path(), "dest", "topic");

    let config = stdout_str(&git(work.path(), &["config", "--list", "--name-only"]));
    assert!(
        config.contains("branch.topic.extra.remote"),
        "the neighbour keeps its upstream; got: {config}"
    );
    assert!(
        !branch_listing(work.path()).contains("topic\n"),
        "and the proven branch still went"
    );
}

/// The pinned delete is plumbing, and `git update-ref -d` will happily remove a
/// branch some worktree has checked out — which `git branch -D` refuses, leaving
/// that worktree pointing at nothing. A worktree that appears while the picker is
/// open is the "became risky after the warning" case, and it must still meet a
/// guard.
#[test]
fn a_worktree_taken_on_the_proven_branch_mid_prompt_saves_it() {
    let (_bare, parent, work) = setup_with_parent();

    push_topic_branch(&work, "feature");
    squash_merge_upstream(&work, "feature");
    git(&work, &["branch", "dest", "main"]);
    let held = parent.path().join("held");

    let raw = drive_cleanup_prompt(&work, "dest", "feature", false, || {
        git(
            &work,
            &["worktree", "add", held.to_str().unwrap(), "feature"],
        );
    });
    let text = String::from_utf8_lossy(&raw);

    assert!(
        branch_listing(&work).contains("feature"),
        "a branch a worktree now holds must survive: {text}"
    );
}

/// Equivalence only ever subtracts. A branch cut from the anchor and never
/// committed to has nothing the anchor lacks, and reading that as "landed"
/// would offer it for deletion the moment it was created.
#[test]
fn an_untouched_branch_cut_from_the_anchor_is_not_read_as_landed() {
    let (_bare, work) = setup();

    git(work.path(), &["branch", "fresh", "main"]);

    let _cwd = cwd_at(work.path());
    assert!(
        !stale_names("origin").contains(&"fresh".to_string()),
        "an untouched branch is not stale, got: {:?}",
        stale_names("origin")
    );
    assert!(
        perch::git::equivalent_branches(None, "origin", &["fresh"]).is_empty(),
        "and an empty diff proves nothing, so equivalence cannot offer it either"
    );
}

// ---------------------------------------------------------------------------
// Worktree hooks
// ---------------------------------------------------------------------------

#[test]
fn br_rm_does_not_fire_the_worktree_removal_hook() {
    let (_bare, parent, work) = setup_with_parent();
    let log = parent.path().join("hook.log");
    let script = format!("printf removed >> '{}'", log.display());
    git(&work, &["config", "perch.hook.removed", &script]);
    git(&work, &["branch", "feature"]);

    let output = perch_hooked(&work, &["br", "rm", "feature"]);

    assert!(output.status.success(), "stderr: {}", stderr_str(&output));
    assert!(
        !log.exists(),
        "br rm must not masquerade as worktree removal"
    );
}

#[test]
fn wt_rm_reports_the_removal_before_the_hook_runs() {
    let (_bare, parent, work) = setup_with_parent();
    add_worktree(&work, &parent, "feature");
    git(
        &work,
        &["config", "perch.hook.removed", "printf 'HOOK-RAN\\n' >&2"],
    );

    let output = perch_hooked(&work, &["wt", "rm", "feature", "--force"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let stderr = stderr_str(&output);
    let removal = stderr.find("removed worktree").expect("removal report");
    let hook = stderr.find("HOOK-RAN").expect("hook output");
    assert!(
        removal < hook,
        "the removal report must precede its hook output; got: {stderr}"
    );
}

/// The payload a hook is handed, from both creation arms and from a removal,
/// each firing exactly once for the worktree it describes.
#[test]
fn wt_hooks_report_each_creation_and_removal_once() {
    let (_bare, parent, work) = setup_with_parent();

    let log = parent.path().join("hook.log");
    let script = format!(
        "printf '%s|%s|%s|%s|%s\\n' \"$PERCH_EVENT\" \"$PERCH_BRANCH\" \
         \"$PERCH_MAIN\" \"$PERCH_WORKTREE\" \"$(pwd -P)\" >> '{}'",
        log.display()
    );
    git(&work, &["config", "perch.hook.created", &script]);
    git(&work, &["config", "perch.hook.removed", &script]);

    // An existing branch and a new one take different creation arms; both are
    // creations as far as a hook is concerned.
    git(&work, &["branch", "feature"]);
    for args in [
        ["wt", "feature"].as_slice(),
        ["wt", "brand-new"].as_slice(),
        ["wt", "rm", "feature", "--force"].as_slice(),
    ] {
        let output = perch_hooked(&work, args);
        assert!(
            output.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            stderr_str(&output)
        );
    }

    // Git reports resolved paths, so compare against resolved ones — on macOS
    // a TempDir under /var is really /private/var.
    let main = work.canonicalize().unwrap();
    let worktrees = main.parent().unwrap().join("worktrees").join("repo");
    let line = |event: &str, branch: &str| {
        format!(
            "{event}|{branch}|{}|{}|{}",
            main.display(),
            worktrees.join(branch).display(),
            main.display()
        )
    };

    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec![
            line("created", "feature"),
            line("created", "brand-new"),
            line("removed", "feature"),
        ],
        "each event fires once, from the main worktree, with the full payload"
    );
}

/// A hook is told, never asked: one that fails is warned about and otherwise
/// ignored, and its stderr reaches the user untouched.
#[test]
fn a_failing_wt_hook_leaves_the_worktree_and_the_handoff_intact() {
    let (_bare, parent, work) = setup_with_parent();

    git(
        &work,
        &[
            "config",
            "perch.hook.created",
            "echo 'hook says no' >&2; exit 3",
        ],
    );

    let output = perch_hooked(&work, &["wt", "feature"]);
    assert!(
        output.status.success(),
        "a failing hook must not fail the command; stderr: {}",
        stderr_str(&output)
    );

    let expected = parent.path().join("worktrees").join("repo").join("feature");
    assert!(
        expected.exists(),
        "worktree should exist at {}",
        expected.display()
    );
    assert!(
        stdout_str(&output).trim().ends_with("repo/feature"),
        "stdout should still be the worktree path; got: {}",
        stdout_str(&output)
    );
    assert!(
        stderr_str(&output).contains("hook says no"),
        "hook stderr should pass through; got: {}",
        stderr_str(&output)
    );
    assert!(
        stderr_str(&output).contains("created hook exited 3"),
        "a non-zero exit should be warned about; got: {}",
        stderr_str(&output)
    );
}

/// The shell wrapper reads the destination path off stdout, so a hook that
/// talks is diverted to stderr rather than being allowed to send the user
/// somewhere absurd.
#[test]
fn a_chatty_wt_hook_cannot_corrupt_the_handoff() {
    let (_bare, _parent, work) = setup_with_parent();

    git(
        &work,
        &["config", "perch.hook.created", "echo /somewhere/else"],
    );

    let output = perch_hooked(&work, &["wt", "feature"]);
    assert!(output.status.success(), "stderr: {}", stderr_str(&output));

    let stdout = stdout_str(&output);
    let printed: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        printed.len(),
        1,
        "stdout should carry the handoff path alone; got: {stdout}"
    );
    assert!(
        printed[0].ends_with("repo/feature"),
        "stdout should be the worktree path; got: {}",
        printed[0]
    );
    assert!(
        stderr_str(&output).contains("/somewhere/else"),
        "hook stdout should be re-emitted on stderr; got: {}",
        stderr_str(&output)
    );
}

/// A stale branch held by a worktree takes that worktree with it, which is as
/// much a removal as `wt rm` is — so the hook fires there too. Without it,
/// `perch wt <branch>` could announce a creation and then silently destroy
/// a different worktree in the same breath. The prompt is interactive, so this
/// drives it over a real pty.
#[test]
fn a_stale_branch_taking_its_worktree_fires_the_removal_hook() {
    let (_bare, work) = setup();

    // Published, then its upstream deleted: stale, and holding a commit of its
    // own so the row carries a marker that licenses deleting it.
    git(work.path(), &["checkout", "-b", "wip"]);
    fs::write(work.path().join("wip.txt"), "x\n").unwrap();
    git(work.path(), &["add", "wip.txt"]);
    git(work.path(), &["commit", "-m", "wip"]);
    git(work.path(), &["push", "-u", "origin", "wip"]);
    git(work.path(), &["push", "origin", "--delete", "wip"]);
    git(work.path(), &["checkout", "main"]);
    git(work.path(), &["fetch", "--prune", "origin"]);
    git(work.path(), &["branch", "dest", "main"]);

    let parent = TempDir::new().unwrap();
    let worktree = parent.path().join("wt");
    git(
        work.path(),
        &["worktree", "add", worktree.to_str().unwrap(), "wip"],
    );

    let log = parent.path().join("hook.log");
    let script = format!(
        "printf '%s|%s|%s\\n' \"$PERCH_EVENT\" \"$PERCH_BRANCH\" \
         \"$PERCH_WORKTREE\" >> '{}'",
        log.display()
    );
    git(work.path(), &["config", "perch.hook.removed", &script]);

    drive_cleanup_prompt(work.path(), "dest", "wip", true, || {});

    assert!(
        !worktree.exists(),
        "the held worktree should be gone: {}",
        worktree.display()
    );
    // Resolve through the parent: the worktree itself is gone by now, and git
    // reports the path it resolved (on macOS, /private/var for a /var TempDir).
    let resolved = parent.path().canonicalize().unwrap().join("wt");
    let logged = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        logged.trim(),
        format!("removed|wip|{}", resolved.display()),
        "the stale prompt should report the worktree it removed; got: {logged}"
    );
}

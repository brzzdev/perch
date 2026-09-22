//! The fetch `wt` starts before the picker opens, so the transport round trip
//! overlaps the local reads and the wait for a keystroke instead of following
//! them.

use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use indicatif::ProgressBar;

use super::{CursorGuard, report_fetch_failure};
use crate::{AppResult, git};

/// How long git gets to act on SIGTERM before the group is killed outright.
/// Removing its lock files takes it milliseconds; the rest of the second is
/// for a loaded machine.
const TERMINATION_GRACE: Duration = Duration::from_secs(1);

/// Gap between checks that git has exited, short enough that an ordinary exit
/// is not perceptibly delayed.
const TERMINATION_POLL: Duration = Duration::from_millis(10);

/// The process group of the background fetch, or `0` when none is running.
/// One slot, because a run starts at most one [`Prefetch`].
///
/// A real SIGINT leaves through the handler in `main`, which calls
/// `process::exit` and so unwinds nothing: [`Prefetch::drop`] never runs, and
/// a fetch that leads its own session would outlive `perch` holding ref locks.
/// The handler reaches the fetch through this instead. It is cleared before
/// the child is reaped, so a signal can never name a group id the kernel has
/// since handed to someone else.
static ACTIVE_GROUP: AtomicI32 = AtomicI32::new(0);

/// Whether the run is being interrupted, so that [`Prefetch::join`] neither
/// answers a fetch the interrupt just killed by starting another one, nor
/// carries on into the work the run was going to do.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Held while the fetch is signalled, and while it is reaped, so that the two
/// cannot interleave across threads. Reaping frees the process group id for
/// reuse, so a reap that landed between an interrupt's SIGTERM and its SIGKILL
/// would leave the SIGKILL naming whatever got the id next.
static SIGNALLING: Mutex<()> = Mutex::new(());

/// Poisoning is nothing to recover from here: the mutex guards no data, only
/// the order of two operations on a child process.
fn signalling() -> MutexGuard<'static, ()> {
    SIGNALLING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a remote name resolves where a fetch runs: the URL it fetches from,
/// with any `insteadOf` rewrite applied, and the whole config the fetch runs
/// under. `extensions.worktreeConfig` and `includeIf` can differ any of these
/// from one worktree to the next, so two fetches of one name are the same
/// fetch only when the whole of this matches. The config is compared wholesale
/// because refspecs, `fetch.*`, transport and credential settings all shape a
/// fetch, and naming the ones that matter would let a key nobody thought of
/// quietly suppress a fetch that was not covered. A worktree whose config
/// differs in some unrelated key costs one extra fetch, nothing more.
#[derive(Default, PartialEq)]
struct FetchContext {
    config: Vec<String>,
    url: Option<String>,
}

impl FetchContext {
    fn read(dir: Option<&Path>, remote: &str) -> Self {
        Self {
            config: git::config_entries(dir),
            url: git::remote_url(dir, remote),
        }
    }

    /// Whether `remote` fetches one repository here whichever directory git
    /// runs in. Never where `remote.<name>.vcs` picks a helper, which gets the
    /// URL, if any, only as an argument to read as it likes, nor where there
    /// is no URL to judge, nor where the fetch runs a program of the user's
    /// choosing or anything else git finds from where it runs. Otherwise it is
    /// down to the URL.
    ///
    /// Such a program is a shell command, so a relative path anywhere in it,
    /// behind `env` or as `sh`'s script, is found from where git runs. Rather
    /// than parse shell, any such setting at all declines coverage, at the
    /// price of one extra fetch for a setup that has one. A relative
    /// `core.hooksPath` declines it too: its `reference-transaction` hook can
    /// reject the fetch's ref updates.
    ///
    /// Accepted limits, each costing a fetch that is wrongly skipped rather
    /// than one wrongly run: fetch-affecting settings git adds later; absolute
    /// hooks that behave differently per directory; refspecs whose
    /// destination is a per-worktree namespace (`refs/worktree/`,
    /// `refs/bisect/`, `refs/rewritten/`); and relative credential and TLS
    /// paths (`core.askPass`, `GIT_ASKPASS`, `http.sslCAInfo`,
    /// `http.sslCert`, `http.sslKey`, `http.cookieFile`), which decide
    /// whether authentication succeeds rather than which repository is
    /// fetched.
    fn names_one_repository(&self, remote: &str) -> bool {
        let upload_pack = format!("remote.{remote}.uploadpack");
        let vcs = format!("remote.{remote}.vcs");
        let config_resolves_per_directory = self.config.iter().any(|entry| {
            // The key, then a newline and the value where it has one.
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            match key {
                "core.gitproxy" | "core.sshcommand" => true,
                // `~` is expanded, so it is as absolute as a leading `/`.
                "core.hookspath" => !value.starts_with(['/', '~']),
                key if key == upload_pack || key == vcs => true,
                // Only the `!` form is a shell command; any other value names
                // a `git credential-*` helper or an absolute path.
                key if key.starts_with("credential.") && key.ends_with(".helper") => {
                    value.starts_with('!')
                }
                _ => false,
            }
        });
        !config_resolves_per_directory
            && !environment_resolves_per_directory()
            && self.url.as_deref().is_some_and(names_one_repository)
    }
}

/// Whether the environment has git find anything from the directory it runs
/// in. The environment is the same for both fetches, but what it names is
/// found from each one's directory all the same: a program, a relative `PATH`
/// entry where `ssh` and helpers are looked up, or a relative `GIT_EXEC_PATH`,
/// which git puts first on `PATH`. A variable that picks the repository itself
/// counts whatever its value.
fn environment_resolves_per_directory() -> bool {
    let runs_a_program = PROGRAM_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|program| !program.is_empty()));
    // An empty `PATH` entry means the current directory.
    let relative_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry.is_relative()));
    let relative_exec_path = std::env::var_os("GIT_EXEC_PATH")
        .is_some_and(|exec_path| Path::new(&exec_path).is_relative());
    let picks_the_repository = REPOSITORY_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    runs_a_program || relative_path || relative_exec_path || picks_the_repository
}

/// The environment variables through which a fetch runs a program of the
/// user's choosing, in place of the config settings of the same purpose.
const PROGRAM_VARIABLES: [&str; 3] = ["GIT_PROXY_COMMAND", "GIT_SSH", "GIT_SSH_COMMAND"];

/// The environment variables that choose the repository, or part of it, in
/// place of the directory git runs in.
const REPOSITORY_VARIABLES: [&str; 5] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_WORK_TREE",
];

/// A remote this run has already fetched and reported on, handed to the steps
/// that would otherwise fetch it again. A failed fetch earns the token too:
/// retrying seconds later from the same directory gains nothing and would
/// print the same warning twice.
pub(crate) struct FetchedRemote {
    context: FetchContext,
    /// What the fetch reported, where it failed.
    failure: Option<String>,
    name: String,
}

impl FetchedRemote {
    /// Whether the fetch already done stands in for a fetch of `remote` from
    /// `dir`. A fetch from the directory this one ran in is covered by name;
    /// one from a worktree is covered only where this fetch succeeded, where
    /// that worktree resolves the name to the same [`FetchContext`], and where
    /// that context names one repository whichever directory git runs in. A
    /// failure covers no worktree: each has a `FETCH_HEAD` of its own, so one
    /// can fetch where another could not. Nor does a fetch cover a worktree
    /// with submodules, since each worktree keeps its own submodule
    /// repositories and a fetch recurses only into those where it runs.
    fn covers(&self, dir: Option<&Path>, remote: &str) -> bool {
        if self.name != remote {
            return false;
        }
        let Some(dir) = dir else {
            return true;
        };
        self.failure.is_none()
            && !dir.join(".gitmodules").exists()
            && self.context.names_one_repository(remote)
            && FetchContext::read(Some(dir), remote) == self.context
    }
}

/// The `scheme://` URLs git fetches itself, or through the helpers it ships.
/// Git matches these case-sensitively; any other scheme goes to a
/// `git-remote-<scheme>` found on `PATH`.
const BUILT_IN_SCHEMES: [&str; 9] = [
    "file", "ftp", "ftps", "git", "git+ssh", "http", "https", "ssh", "ssh+git",
];

/// Whether `url` names the same repository whichever directory git fetches it
/// from, by git's own reading of a URL. A URL in one of the
/// [`BUILT_IN_SCHEMES`], an absolute path and scp-style `host:path` do. A
/// relative path does not, since git resolves it against the directory it runs
/// in. Nor can a URL for any other helper be trusted to, whether written
/// `<transport>::<address>` or `<scheme>://<address>`: the helper, `ext::`
/// included, is free to read the address as a path relative to where it runs.
fn names_one_repository(url: &str) -> bool {
    // A leading run of scheme characters, as git reads a transport name.
    let scheme = url
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .unwrap_or(url.len());
    let rest = &url[scheme..];
    if scheme > 0 && rest.starts_with("::") {
        return false;
    }
    if rest.starts_with("://") {
        return BUILT_IN_SCHEMES.contains(&&url[..scheme]);
    }
    if url.starts_with('/') {
        return true;
    }
    // A `:` before any `/` makes it scp-style, and anything else local.
    match (url.find(':'), url.find('/')) {
        (Some(colon), slash) => slash.is_none_or(|slash| colon < slash),
        (None, _) => false,
    }
}

/// Fetches `remote` from `dir` unless `fetched` already covers it. The outcome
/// is what is left to report, so it reads as a success where `fetched` covers
/// the fetch, and where this one fails just as `fetched` did and that warning
/// has already been printed.
pub(crate) fn fetch_unless_covered(
    dir: Option<&Path>,
    remote: &str,
    fetched: Option<&FetchedRemote>,
) -> git::FetchOutcome {
    if fetched.is_some_and(|fetched| fetched.covers(dir, remote)) {
        return git::FetchOutcome::Ok;
    }
    match git::fetch(dir, remote) {
        git::FetchOutcome::Failed(detail)
            if fetched.is_some_and(|fetched| {
                fetched.name == remote && fetched.failure.as_ref() == Some(&detail)
            }) =>
        {
            git::FetchOutcome::Ok
        }
        outcome => outcome,
    }
}

/// Ends the background fetch, for a caller about to leave without unwinding.
/// A no-op where none is running. Never reaps: the [`Child`] belongs to the
/// thread that started it, and the process is exiting, so what is left is
/// reparented and reaped by init.
///
/// This waits and sleeps, neither of which would be safe inside a signal
/// handler. It is called from one only in the sense that `ctrlc` runs its
/// handler on a thread of its own rather than on the interrupted stack.
pub(crate) fn terminate_active() {
    let _signalling = signalling();
    INTERRUPTED.store(true, Ordering::SeqCst);
    let group = ACTIVE_GROUP.swap(0, Ordering::SeqCst);
    if group != 0 {
        end_group(group);
    }
}

/// A `git fetch` running in the background, ended and reaped on drop unless
/// [`Prefetch::join`] took its result first. Esc, Ctrl-C and every error on
/// the way to the join therefore end the child rather than leave it to finish
/// after `perch` has exited. Picker Ctrl-C arrives as an `Interrupted` error
/// rather than a signal, so the drop is all that reaches the child there; a
/// real SIGINT skips the drop entirely and goes through [`terminate_active`].
pub(crate) struct Prefetch {
    child: Option<Child>,
    context: FetchContext,
    remote: String,
}

impl Prefetch {
    /// Starts the fetch. Failing to spawn it is not worth stopping for: the
    /// join then fetches in the foreground, as every run did before.
    pub(crate) fn start(remote: &str) -> Self {
        let child = spawn_unless_interrupted(remote);
        // Read after the spawn, so the lookups overlap the fetch.
        let context = FetchContext::read(None, remote);
        Self {
            child,
            context,
            remote: remote.to_string(),
        }
    }

    /// Waits for the fetch behind a spinner. A background fetch that failed is
    /// retried in the foreground with the user's own environment, so a
    /// passphrase or credential prompt can be answered; only that final
    /// outcome is reported.
    pub(crate) fn join(mut self) -> AppResult<FetchedRemote> {
        let remote = std::mem::take(&mut self.remote);
        let outcome = {
            let spinner = ProgressBar::new_spinner().with_message(format!("Fetching {remote}…"));
            let _cursor_guard = CursorGuard::hide();
            spinner.enable_steady_tick(Duration::from_millis(80));
            let succeeded = self.child.take().is_some_and(|mut child| {
                wait_for_exit(&child);
                let _signalling = signalling();
                // An interrupt owns the fetch from the moment it is flagged.
                // Leaving the child unreaped is what keeps its group id
                // reserved until the interrupt's last signal has landed.
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return false;
                }
                // Cleared before the reap below frees the id, so an interrupt
                // arriving now finds nothing to signal rather than a stranger.
                ACTIVE_GROUP.store(0, Ordering::SeqCst);
                child.wait().is_ok_and(|status| status.success())
            });
            let outcome = if succeeded || INTERRUPTED.load(Ordering::SeqCst) {
                // An interrupted fetch is one the user just ended. Retrying it
                // would start fresh work on the way out, and that retry runs in
                // this process's own group, where nothing would clean it up.
                git::FetchOutcome::Ok
            } else {
                git::fetch(None, &remote)
            };
            spinner.finish_and_clear();
            outcome
        };
        report_fetch_failure(&outcome);
        let failure = match outcome {
            git::FetchOutcome::Ok => None,
            git::FetchOutcome::Failed(detail) => Some(detail),
        };
        // The run stops here rather than going on to make a worktree and fire
        // its hooks: the interrupt is the user asking for none of that, and
        // the handler is meanwhile seeing the fetch off.
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted").into());
        }
        Ok(FetchedRemote {
            context: std::mem::take(&mut self.context),
            failure,
            name: remote,
        })
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _signalling = signalling();
        match group_of(&child) {
            Some(group) => end_group(group),
            // Nothing to signal as a group, so git alone — rather than an
            // unbounded wait on a child that may ignore SIGTERM.
            None => {
                let _ = child.kill();
            }
        }
        // Cleared before the reap below frees the id for reuse.
        ACTIVE_GROUP.store(0, Ordering::SeqCst);
        let _ = child.wait();
    }
}

/// Spawns the fetch and publishes its group under the lock [`terminate_active`]
/// takes, so an interrupt either finds the group to end or lands first and
/// nothing is spawned. Were the two apart, an interrupt between them would find
/// no group while the fetch was already running, and leave it behind when the
/// process exits.
fn spawn_unless_interrupted(remote: &str) -> Option<Child> {
    let _signalling = signalling();
    if INTERRUPTED.load(Ordering::SeqCst) {
        return None;
    }
    let child = git::fetch_in_background(remote).ok()?;
    if let Some(group) = group_of(&child) {
        ACTIVE_GROUP.store(group, Ordering::SeqCst);
    }
    Some(child)
}

/// Ends every process in `group`. SIGTERM first, because git removes its lock
/// files on SIGTERM and SIGKILL would leave a `refs/remotes/*.lock` behind for
/// the next fetch to trip over. Then SIGKILL, unconditionally, because git can
/// be gone while an `ssh` or remote helper that trapped SIGTERM lives on in the
/// group. The fetch led its own session from the start, so its pid names the
/// group.
///
/// The grace lasts only until git itself exits. Git holds the locks, so once
/// it has gone nothing is left that needs the time. Probing the group with
/// `kill(-group, 0)` would not tell anyway: on Linux the unreaped leader stays
/// in the group as a zombie, so the group never looks empty. Git is checked without being
/// reaped, which is what keeps the SIGKILL safe. Reaping would free its id,
/// and the kernel could give that id to an unrelated new group before the
/// signal lands.
fn end_group(group: i32) {
    signal_group(group, Signal::Term);
    let deadline = Instant::now() + TERMINATION_GRACE;
    while !leader_has_exited(group) && Instant::now() < deadline {
        std::thread::sleep(TERMINATION_POLL);
    }
    signal_group(group, Signal::Kill);
}

#[derive(Clone, Copy)]
enum Signal {
    Kill,
    Term,
}

/// Blocks until the fetch has exited, leaving it unreaped — `WNOWAIT` reports
/// the exit without consuming it. Reaping is what frees a pid for reuse, and
/// [`ACTIVE_GROUP`] still names this one, so the caller clears that first and
/// reaps second. What the wait reports is discarded: the caller reads the
/// status from the reap.
#[cfg(unix)]
fn wait_for_exit(child: &Child) {
    let pid: libc::id_t = child.id();
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: `info` is a live, aligned allocation of exactly the
        // `siginfo_t` the call writes, and nothing reads it afterwards.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        // A signal landing mid-wait is not an answer about the child.
        if waited == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return;
        }
    }
}

#[cfg(unix)]
fn group_of(child: &Child) -> Option<i32> {
    i32::try_from(child.id()).ok()
}

#[cfg(unix)]
fn signal_group(group: i32, signal: Signal) {
    let signal = match signal {
        Signal::Kill => libc::SIGKILL,
        Signal::Term => libc::SIGTERM,
    };
    // SAFETY: plain signal delivery to a process group this process created.
    unsafe { libc::kill(-group, signal) };
}

/// Whether the fetch leading `group` has exited, without reaping it —
/// `WNOHANG` returns at once and `WNOWAIT` leaves the exit for the caller's
/// own reap. Only `ECHILD`, no such child left to wait for, reads as an exit.
/// Any other failure says nothing about git, so it reads as still running and
/// the grace runs on: ending it early would SIGKILL git mid-way through
/// removing its lock files.
#[cfg(unix)]
fn leader_has_exited(group: i32) -> bool {
    let Ok(pid) = libc::id_t::try_from(group) else {
        return true;
    };
    loop {
        // Zeroed, because `WNOHANG` may leave it untouched when nothing has
        // exited, and a zero `si_pid` is how that case reads.
        // SAFETY: `siginfo_t` is plain C data, for which all zeroes is valid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live, aligned `siginfo_t`, the one the call
        // writes.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited == 0 {
            // SAFETY: a successful `waitid` has filled in `si_pid` or left it
            // zeroed.
            return unsafe { info.si_pid() } != 0;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ECHILD) => return true,
            _ => return false,
        }
    }
}

#[cfg(not(unix))]
fn wait_for_exit(_child: &Child) {}

#[cfg(not(unix))]
fn group_of(_child: &Child) -> Option<i32> {
    None
}

#[cfg(not(unix))]
fn signal_group(_group: i32, _signal: Signal) {}

#[cfg(not(unix))]
fn leader_has_exited(_group: i32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::*;

    /// On Linux a probe of the whole group sees the unreaped leader and never
    /// finds it empty, so every cleanup sat out the full grace.
    #[test]
    fn a_group_that_goes_on_sigterm_ends_without_waiting_out_the_grace() {
        let mut child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = group_of(&child).unwrap();

        let started = Instant::now();
        end_group(group);
        let took = started.elapsed();
        let _ = child.wait();

        assert!(
            took < TERMINATION_GRACE / 2,
            "ending the group took {took:?}"
        );
    }
}

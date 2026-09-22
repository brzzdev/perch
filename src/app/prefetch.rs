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

/// Gap between checks that the group has gone, short enough that an ordinary
/// exit is not perceptibly delayed.
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
/// with any `insteadOf` rewrite applied, and the settings that shape what the
/// fetch brings back. `extensions.worktreeConfig` and `includeIf` can differ
/// any of these from one worktree to the next, so two fetches of one name are
/// the same fetch only when the whole of this matches. Comparing the settings
/// wholesale rather than naming the keys that matter keeps a key nobody
/// thought of from quietly suppressing a fetch that was not covered.
#[derive(Default, PartialEq)]
struct FetchContext {
    settings: Vec<String>,
    url: Option<String>,
}

impl FetchContext {
    fn read(dir: Option<&Path>, remote: &str) -> Self {
        Self {
            settings: git::remote_settings(dir, remote),
            url: git::remote_url(dir, remote),
        }
    }
}

/// A remote this run has already fetched and reported on, handed to the steps
/// that would otherwise fetch it again. A failed fetch earns the token too:
/// retrying seconds later from the same environment gains nothing and would
/// print the same warning twice.
pub(crate) struct FetchedRemote {
    context: FetchContext,
    name: String,
}

impl FetchedRemote {
    /// Whether the fetch already done stands in for a fetch of `remote` from
    /// `dir`. A fetch from the directory this one ran in is covered by name;
    /// one from a worktree is covered only where that worktree resolves the
    /// name to the same [`FetchContext`].
    fn covers(&self, dir: Option<&Path>, remote: &str) -> bool {
        self.name == remote
            && dir.is_none_or(|dir| FetchContext::read(Some(dir), remote) == self.context)
    }
}

/// Fetches `remote` from `dir` unless `fetched` already covers it, in which
/// case there is nothing left to report and the outcome reads as a success.
pub(crate) fn fetch_unless_covered(
    dir: Option<&Path>,
    remote: &str,
    fetched: Option<&FetchedRemote>,
) -> git::FetchOutcome {
    if fetched.is_some_and(|fetched| fetched.covers(dir, remote)) {
        return git::FetchOutcome::Ok;
    }
    git::fetch(dir, remote)
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
        let child = git::fetch_in_background(remote).ok();
        if let Some(group) = child.as_ref().and_then(group_of) {
            ACTIVE_GROUP.store(group, Ordering::SeqCst);
        }
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
        // The run stops here rather than going on to make a worktree and fire
        // its hooks: the interrupt is the user asking for none of that, and
        // the handler is meanwhile seeing the fetch off.
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted").into());
        }
        Ok(FetchedRemote {
            context: std::mem::take(&mut self.context),
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

/// Ends every process in `group`. SIGTERM first, because git removes its lock
/// files on SIGTERM and SIGKILL would leave a `refs/remotes/*.lock` behind for
/// the next fetch to trip over. Then SIGKILL, once the group has had its grace
/// and unconditionally, because git can be gone while an `ssh` or remote
/// helper that trapped SIGTERM lives on in the group. The fetch led its own
/// session from the start, so its pid names the group.
///
/// Waiting on the group rather than on the child is what keeps the SIGKILL
/// safe: reaping the leader first would free its id, and the kernel could hand
/// the same one to an unrelated new group before the signal lands.
fn end_group(group: i32) {
    signal_group(group, Signal::Term);
    let deadline = Instant::now() + TERMINATION_GRACE;
    while group_is_alive(group) && Instant::now() < deadline {
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

/// Whether anything in `group` is still running. Signal `0` asks without
/// sending, and fails once every member has exited — an unreaped leader is a
/// zombie, which is no longer a member.
#[cfg(unix)]
fn group_is_alive(group: i32) -> bool {
    // SAFETY: the null signal only probes for the group's existence.
    unsafe { libc::kill(-group, 0) == 0 }
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
fn group_is_alive(_group: i32) -> bool {
    false
}

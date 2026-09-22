//! The fetch `wt` starts before the picker opens, so the transport round trip
//! overlaps the local reads and the wait for a keystroke instead of following
//! them.

use std::path::Path;
use std::process::Child;
use std::time::Duration;

use indicatif::ProgressBar;

use super::{CursorGuard, report_fetch_failure};
use crate::git;

/// A remote this run has already fetched and reported on, handed to the steps
/// that would otherwise fetch it again. A failed fetch earns the token too:
/// retrying seconds later from the same environment gains nothing and would
/// print the same warning twice.
pub(crate) struct FetchedRemote(String);

/// Fetches `remote` unless `fetched` already covers it, in which case there
/// is nothing left to report and the outcome reads as a success.
pub(crate) fn fetch_unless_covered(
    dir: Option<&Path>,
    remote: &str,
    fetched: Option<&FetchedRemote>,
) -> git::FetchOutcome {
    if fetched.is_some_and(|fetched| fetched.0 == remote) {
        return git::FetchOutcome::Ok;
    }
    git::fetch(dir, remote)
}

/// A `git fetch` running in the background, ended and reaped on drop unless
/// [`Prefetch::join`] took its result first. Esc, Ctrl-C and every error on
/// the way to the join therefore end the child rather than leave it to finish
/// after `perch` has exited. Picker Ctrl-C arrives as an `Interrupted` error,
/// not a SIGINT, so nothing else would signal it.
pub(crate) struct Prefetch {
    child: Option<Child>,
    remote: String,
}

impl Prefetch {
    /// Starts the fetch. Failing to spawn it is not worth stopping for: the
    /// join then fetches in the foreground, as every run did before.
    pub(crate) fn start(remote: &str) -> Self {
        Self {
            child: git::fetch_in_background(remote).ok(),
            remote: remote.to_string(),
        }
    }

    /// Waits for the fetch behind a spinner. A background fetch that failed is
    /// retried in the foreground with the user's own environment, so a
    /// passphrase or credential prompt can be answered; only that final
    /// outcome is reported.
    pub(crate) fn join(mut self) -> FetchedRemote {
        let remote = std::mem::take(&mut self.remote);
        let outcome = {
            let spinner = ProgressBar::new_spinner().with_message(format!("Fetching {remote}…"));
            let _cursor_guard = CursorGuard::hide();
            spinner.enable_steady_tick(Duration::from_millis(80));
            let succeeded = self
                .child
                .take()
                .is_some_and(|mut child| child.wait().is_ok_and(|status| status.success()));
            let outcome = if succeeded {
                git::FetchOutcome::Ok
            } else {
                git::fetch(None, &remote)
            };
            spinner.finish_and_clear();
            outcome
        };
        report_fetch_failure(&outcome);
        FetchedRemote(remote)
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        terminate(&mut child);
    }
}

/// Ends the fetch and reaps it. SIGTERM rather than SIGKILL, because git
/// removes its lock files on SIGTERM and SIGKILL would leave a
/// `refs/remotes/*.lock` behind for the next fetch to trip over; and to the
/// whole process group rather than git alone, so the `ssh` or remote helper
/// it spawned goes with it. The child led its own session from the start, so
/// its pid names the group. Should the group signal miss all the same, git
/// alone is killed rather than waited on forever.
fn terminate(child: &mut Child) {
    // SAFETY: plain signal delivery to a process group this process created
    // and has not yet reaped.
    let signalled = libc::pid_t::try_from(child.id())
        .is_ok_and(|pid| unsafe { libc::kill(-pid, libc::SIGTERM) } == 0);
    if !signalled {
        let _ = child.kill();
    }
    let _ = child.wait();
}

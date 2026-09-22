//! Starting a child process in a session of its own.

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes the child lead a new session. That takes it out of the invoking
/// session, so a manager sweeping that session on close cannot reach it
/// (ADR 0010), and leaves it with no controlling terminal, so nothing it runs
/// can prompt. It also makes the child the leader of its own process group, so
/// its pid names the group when a signal has to reach everything it spawned.
///
/// `process_group(0)` must not be set alongside this: std runs `setpgid`
/// before `pre_exec`, and `setsid` then fails with EPERM.
pub(crate) fn detach(command: &mut Command) {
    // SAFETY: `setsid` is async-signal-safe and touches no memory shared with
    // the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

//! Starting a child process in a session of its own.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

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

/// The process group of the command [`output_detached`] is running, or `0`.
static FOREGROUND_GROUP: AtomicI32 = AtomicI32::new(0);

/// Held while that group is killed, and while its leader is reaped, so a kill
/// can never name a group id the kernel has since handed to someone else.
static REAPING: Mutex<()> = Mutex::new(());

fn reaping() -> MutexGuard<'static, ()> {
    REAPING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `command` to completion, as [`Command::output`] does, but leading a
/// session of its own, so it has no terminal to prompt on. A terminal's
/// Ctrl-C no longer reaches it, so [`end_foreground`] has to.
pub(crate) fn output_detached(command: &mut Command) -> std::io::Result<Output> {
    detach(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = {
        let _reaping = reaping();
        let child = command.spawn()?;
        FOREGROUND_GROUP.store(i32::try_from(child.id()).unwrap_or(0), Ordering::SeqCst);
        child
    };
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)?;
    }
    let stderr = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    wait_for_exit(&child);
    let _reaping = reaping();
    FOREGROUND_GROUP.store(0, Ordering::SeqCst);
    let status = child.wait()?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Kills whatever [`output_detached`] is running, for a caller about to exit
/// without unwinding. It only ever reads, so there is no lock to let it clear.
pub(crate) fn end_foreground() {
    let _reaping = reaping();
    let group = FOREGROUND_GROUP.swap(0, Ordering::SeqCst);
    if group != 0 {
        // SAFETY: plain signal delivery to a process group this process created.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
}

/// Blocks until `child` has exited, leaving it unreaped — `WNOWAIT` reports
/// the exit without consuming it. Reaping is what frees a pid for reuse, so a
/// caller that has published the pid as a group to signal clears that first
/// and reaps second. What the wait reports is discarded: the caller reads the
/// status from the reap.
pub(crate) fn wait_for_exit(child: &Child) {
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

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn end_foreground_kills_a_detached_command_that_is_still_running() {
        let ender = std::thread::spawn(|| {
            while FOREGROUND_GROUP.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            end_foreground();
        });
        let started = Instant::now();

        let output = output_detached(Command::new("sleep").arg("60")).unwrap();

        ender.join().unwrap();
        assert!(!output.status.success(), "the kill should end it");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

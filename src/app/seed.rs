//! Seeding a new worktree with copy-on-write clones of the main worktree's
//! build caches, the paths configured as `perch.copy`.
//!
//! Only a real clone is ever made. Where the filesystem can't clone, the path
//! is skipped with a warning rather than deep-copied, since a byte-for-byte
//! copy of a `target/` can fill the disk.

use std::io;
use std::path::{Component, Path};

use super::{spinner, warn};
use crate::git;

/// Clones each `perch.copy` path that exists in `main` into the same place in
/// `worktree`. Every failure is a warning: the worktree is usable without its
/// caches, only slower to build.
pub(crate) fn seed(main: &Path, worktree: &Path) {
    for entry in git::config_values("perch.copy") {
        if let Err(reason) = seed_one(main, worktree, &entry) {
            eprintln!("{} not copying {entry}: {reason}", warn());
        }
    }
}

fn seed_one(main: &Path, worktree: &Path, entry: &str) -> Result<(), String> {
    let relative = Path::new(entry);
    if !stays_inside(relative) {
        return Err("it is not a path inside the repository".into());
    }
    let source = main.join(relative);
    if source.symlink_metadata().is_err() {
        return Ok(());
    }
    let destination = worktree.join(relative);
    if destination.symlink_metadata().is_ok() {
        return Err("it already exists in the new worktree".into());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let (spinner, _cursor) = spinner(format!("Cloning {entry}…"));
    let result = clone_tree(&source, &destination);
    spinner.finish_and_clear();
    result.map_err(|error| {
        remove_partial(&destination);
        reason(&error)
    })
}

/// A relative path with no `..`, so joined onto either worktree it stays inside.
fn stays_inside(path: &Path) -> bool {
    path.file_name().is_some()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn remove_partial(destination: &Path) {
    let Ok(metadata) = destination.symlink_metadata() else {
        return;
    };
    let _ = if metadata.is_dir() {
        std::fs::remove_dir_all(destination)
    } else {
        std::fs::remove_file(destination)
    };
}

fn reason(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::CrossesDevices {
        return "the new worktree is on another volume, which copy-on-write cannot reach".into();
    }
    error.to_string()
}

/// APFS clones a whole directory tree in one call, symlinks included as links.
#[cfg(target_os = "macos")]
fn clone_tree(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`, which `libc` doesn't export.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both pointers are NUL-terminated paths that outlive the call.
    let status = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), CLONE_NOFOLLOW) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Linux reflinks one file at a time, so the tree is walked by hand.
#[cfg(target_os = "linux")]
fn clone_tree(source: &Path, destination: &Path) -> io::Result<()> {
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

    let metadata = source.symlink_metadata()?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return symlink(fs::read_link(source)?, destination);
    }
    if file_type.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            clone_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        // Set last, so a read-only directory still takes its contents.
        return fs::set_permissions(destination, metadata.permissions());
    }
    if !file_type.is_file() {
        return Ok(());
    }
    let from = File::open(source)?;
    let to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(metadata.permissions().mode())
        .open(destination)?;
    // SAFETY: both descriptors stay open for the duration of the call.
    if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Build tools judge freshness by mtime, which a new file would otherwise reset.
    to.set_modified(metadata.modified()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone_tree(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_path_below_the_root_stays_inside() {
        for inside in ["target", "./target", "web/node_modules"] {
            assert!(stays_inside(Path::new(inside)), "{inside}");
        }
        for outside in ["", ".", "../escape", "web/../../escape", "/etc"] {
            assert!(!stays_inside(Path::new(outside)), "{outside}");
        }
    }
}

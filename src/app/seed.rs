//! Seeding a new worktree with copy-on-write clones of the paths configured as
//! `perch.copy`. Never a byte-for-byte copy, which for a `target/` can fill the disk.

use std::io;
use std::path::{Component, Path, PathBuf};

use super::{spinner, warn};
use crate::git;

/// Clones each `perch.copy` path that exists in `main` into the same place in
/// `worktree`, warning about each one it can't.
pub(crate) fn seed(main: &Path, worktree: &Path) {
    let mut seeded: Vec<PathBuf> = Vec::new();
    for entry in git::config_values("perch.copy") {
        let Some(relative) = inside_path(&entry) else {
            eprintln!(
                "{} not copying {entry}: it is not a path inside the repository",
                warn()
            );
            continue;
        };
        // Config scopes repeat entries, and a nested one arrived with its parent.
        if seeded.iter().any(|done| relative.starts_with(done)) {
            continue;
        }
        match seed_one(main, worktree, &relative) {
            Ok(()) => seeded.push(relative),
            Err(reason) => eprintln!("{} not copying {entry}: {reason}", warn()),
        }
    }
}

fn seed_one(main: &Path, worktree: &Path, relative: &Path) -> Result<(), String> {
    let source = main.join(relative);
    match source.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(_) => {}
    }
    let created = first_missing(worktree, relative)?;
    let destination = worktree.join(relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let (spinner, _cursor) = spinner(format!("Cloning {}…", relative.display()));
    let result = clone_tree(&source, &destination);
    spinner.finish_and_clear();
    result.map_err(|error| {
        remove_partial(&created);
        reason(&error)
    })
}

/// `entry` rebuilt from its parts, without a trailing slash, or `None` where it
/// could leave the repository.
fn inside_path(entry: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for component in Path::new(entry).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => path.push(part),
            _ => return None,
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

/// The first path on the way to `relative` that `worktree` lacks, which is what
/// cloning it creates. A symlink on the way could lead out of the worktree.
fn first_missing(worktree: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut path = worktree.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        path.push(component);
        let Ok(metadata) = path.symlink_metadata() else {
            return Ok(path);
        };
        if metadata.is_symlink() && components.peek().is_some() {
            return Err(format!(
                "{} is a symlink in the new worktree",
                path.display()
            ));
        }
    }
    Err("it already exists in the new worktree".into())
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
/// clonefile(2) points directories at copyfile(3) instead, but that clones file
/// by file, which on a `target/` is the slow path this exists to avoid.
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
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};

    const OWNER_RWX: u32 = 0o700;

    let metadata = source.symlink_metadata()?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return symlink(fs::read_link(source)?, destination);
    }
    if file_type.is_dir() {
        // Owner rwx on top of the source's mode, so a failure part-way can still
        // remove what was cloned.
        fs::DirBuilder::new()
            .mode(metadata.permissions().mode() | OWNER_RWX)
            .create(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            clone_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
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
    fn only_a_path_below_the_root_is_inside() {
        for (entry, inside) in [
            ("./target", "target"),
            ("target/", "target"),
            ("web/node_modules", "web/node_modules"),
        ] {
            assert_eq!(inside_path(entry), Some(PathBuf::from(inside)), "{entry}");
        }
        for outside in ["", ".", "../escape", "web/../../escape", "/etc"] {
            assert_eq!(inside_path(outside), None, "{outside}");
        }
    }
}

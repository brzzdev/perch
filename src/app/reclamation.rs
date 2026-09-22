//! Durable, detached reclamation of removed worktree directories.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{AppResult, Error, git, session};

const CONFIG_KEY: &str = "perch.reclamation.worktree";
const LOCK_FILE: &str = "perch-reclamation.lock";
const TRASH_PREFIX: &str = ".perch-trash.";
const WORKER_ENV: &str = "PERCH_INTERNAL_RECLAMATION";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Staged,
    Ready,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Record {
    state: State,
    original: PathBuf,
    trash: PathBuf,
}

impl Record {
    fn staged(original: &Path, trash: &Path) -> Self {
        Self {
            state: State::Staged,
            original: original.to_path_buf(),
            trash: trash.to_path_buf(),
        }
    }

    fn ready(original: &Path, trash: &Path) -> Self {
        Self {
            state: State::Ready,
            original: original.to_path_buf(),
            trash: trash.to_path_buf(),
        }
    }

    fn encode(&self) -> AppResult<String> {
        let state = match self.state {
            State::Staged => "staged",
            State::Ready => "ready",
        };
        let original = self
            .original
            .to_str()
            .ok_or_else(|| non_utf8(&self.original))?;
        let trash = self.trash.to_str().ok_or_else(|| non_utf8(&self.trash))?;
        Ok(format!(
            "{state}:{}:{}",
            encode_hex(original.as_bytes()),
            encode_hex(trash.as_bytes())
        ))
    }

    fn decode(value: &str) -> Option<Self> {
        let mut fields = value.split(':');
        let state = match fields.next()? {
            "staged" => State::Staged,
            "ready" => State::Ready,
            _ => return None,
        };
        let original = PathBuf::from(String::from_utf8(decode_hex(fields.next()?)?).ok()?);
        let trash = PathBuf::from(String::from_utf8(decode_hex(fields.next()?)?).ok()?);
        if fields.next().is_some() || !valid_record_paths(&original, &trash) {
            return None;
        }
        Some(Self {
            state,
            original,
            trash,
        })
    }
}

#[derive(Debug)]
pub(super) struct Staged {
    pub(super) config: PathBuf,
    pub(super) original: PathBuf,
    pub(super) trash: PathBuf,
    // Holding this advisory lock keeps retries from observing the staged record
    // until Git has either removed the registration or Perch has restored it.
    pub(super) _lock: Option<File>,
}

/// Start a worker for every durable record left by an earlier Removal. Starting
/// workers is best-effort and never waits for a lock or for file reclamation.
pub(super) fn retry() {
    let Ok(config) = repository_config() else {
        return;
    };
    let Ok(records) = configured_records(&config) else {
        return;
    };
    for record in records {
        let _ = spawn_worker(&record);
    }
}

/// Record and move `original` to a hidden sibling on the same volume.
pub(super) fn stage(original: &Path) -> AppResult<Staged> {
    let config = repository_config()?;
    let lock = repository_lock(&config)?;
    let trash = unused_trash_path(original)?;
    let record = Record::staged(original, &trash);
    record_config(&config, &record)?;
    fs::rename(original, &trash)
        .map_err(|error| reclamation_error("move worktree", original, &error))?;
    Ok(Staged {
        config,
        original: original.to_path_buf(),
        trash,
        _lock: Some(lock),
    })
}

/// Put a staged worktree back after Git refused to remove its registration.
pub(super) fn restore(staged: &Staged) -> AppResult<()> {
    fs::rename(&staged.trash, &staged.original)
        .map_err(|error| reclamation_error("restore worktree", &staged.trash, &error))?;
    let _ = forget_config(
        &staged.config,
        &Record::staged(&staged.original, &staged.trash),
    );
    Ok(())
}

/// Mark a staged directory as safe to reclaim and start a detached worker.
pub(super) fn start(staged: &Staged) -> AppResult<()> {
    let staged_record = Record::staged(&staged.original, &staged.trash);
    let ready_record = Record::ready(&staged.original, &staged.trash);

    // Add before removing so a crash can leave a duplicate, but never a gap.
    record_config(&staged.config, &ready_record)?;
    let _ = forget_config(&staged.config, &staged_record);
    spawn_worker(&ready_record)
        .map_err(|error| reclamation_error("start background reclamation", &staged.trash, &error))
}

/// Run the private reclamation worker requested through the process environment.
pub(super) fn run_worker() -> Option<AppResult<()>> {
    let value = std::env::var(WORKER_ENV).ok()?;
    let record = Record::decode(&value)
        .ok_or_else(|| Error::Reclamation("invalid background record".to_string()));
    Some(record.and_then(|record| reclaim(&record)))
}

fn reclaim(requested: &Record) -> AppResult<()> {
    let config = repository_config()?;
    let ready = {
        let _lock = repository_lock(&config)?;
        let records = configured_records(&config)?;
        let Some(record) = records.iter().find(|record| *record == requested) else {
            return Ok(());
        };
        match record.state {
            State::Ready => record.clone(),
            State::Staged => match recover_staged(&config, record)? {
                Some(ready) => ready,
                None => return Ok(()),
            },
        }
    };

    let claim = match File::open(&ready.trash) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = forget_config(&config, &ready);
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    match claim.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(()),
        Err(fs::TryLockError::Error(error)) => return Err(error.into()),
    }

    let status = Command::new("rm")
        .args(["-rf", "--"])
        .arg(&ready.trash)
        .status()?;
    if status.success() {
        let _ = forget_config(&config, &ready);
    }
    Ok(())
}

/// Resolve a crash while a directory was staged. Only a missing original path
/// permits Git deregistration and promotion to a deletable record.
fn recover_staged(config: &Path, staged: &Record) -> AppResult<Option<Record>> {
    if staged.original.exists() {
        if !staged.trash.exists() {
            let _ = forget_config(config, staged);
        }
        return Ok(None);
    }

    match git::worktree_remove(&staged.original, false)? {
        git::WorktreeRemoveOutcome::Removed => {
            let ready = Record::ready(&staged.original, &staged.trash);
            record_config(config, &ready)?;
            let _ = forget_config(config, staged);
            Ok(Some(ready))
        }
        git::WorktreeRemoveOutcome::Failed(_) => Ok(None),
    }
}

fn repository_config() -> AppResult<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()?;
    if !output.status.success() {
        return Err(Error::Git {
            command: "rev-parse --git-common-dir".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("config"))
}

fn repository_lock(config: &Path) -> AppResult<File> {
    let common_dir = config.parent().ok_or_else(|| {
        Error::Reclamation(format!("{} has no parent directory", config.display()))
    })?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(common_dir.join(LOCK_FILE))?;
    file.lock()?;
    Ok(file)
}

fn configured_records(config: &Path) -> AppResult<Vec<Record>> {
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(config)
        .args(["--null", "--get-all", CONFIG_KEY])
        .output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter_map(Record::decode)
            .collect());
    }
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(Error::Git {
        command: "config --get-all".to_string(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn record_config(config: &Path, record: &Record) -> AppResult<()> {
    let value = record.encode()?;
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(config)
        .args(["--add", CONFIG_KEY])
        .arg(value)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Git {
        command: "config --add".to_string(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn forget_config(config: &Path, record: &Record) -> AppResult<()> {
    let value = record.encode()?;
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(config)
        .args(["--fixed-value", "--unset-all", CONFIG_KEY])
        .arg(value)
        .output()?;
    if output.status.success() || output.status.code() == Some(5) {
        return Ok(());
    }
    Err(Error::Git {
        command: "config --unset-all".to_string(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn unused_trash_path(original: &Path) -> AppResult<PathBuf> {
    let parent = original.parent().ok_or_else(|| {
        Error::Reclamation(format!("{} has no parent directory", original.display()))
    })?;
    let name = original
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| non_utf8(original))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0..100_u8 {
        let candidate = parent.join(format!(
            "{TRASH_PREFIX}{name}.{}.{nonce}.{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::Reclamation(format!(
        "could not choose a trash path beside {}",
        original.display()
    )))
}

fn valid_record_paths(original: &Path, trash: &Path) -> bool {
    original.is_absolute()
        && trash.is_absolute()
        && original.parent() == trash.parent()
        && trash
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(TRASH_PREFIX) && name.len() > TRASH_PREFIX.len())
}

fn spawn_worker(record: &Record) -> std::io::Result<()> {
    if !valid_record_paths(&record.original, &record.trash) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing reclamation path {}", record.trash.display()),
        ));
    }
    let value = record
        .encode()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env(WORKER_ENV, value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A new session, so a manager that sweeps the invoking session cannot kill
    // the worker (ADR 0010).
    session::detach(&mut command);
    command.spawn().map(|_| ())
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from((high << 4) | low).ok()
        })
        .collect()
}

fn non_utf8(path: &Path) -> Error {
    Error::Reclamation(format!("non-utf8 worktree path: {}", path.display()))
}

fn reclamation_error(action: &str, recovery: &Path, error: &std::io::Error) -> Error {
    Error::Reclamation(format!(
        "could not {action}: {error}; files remain at {}",
        recovery.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_without_exposing_paths_to_config_syntax() {
        for state in [State::Staged, State::Ready] {
            let record = Record {
                state,
                original: PathBuf::from("/tmp/worktrees/a:b"),
                trash: PathBuf::from("/tmp/worktrees/.perch-trash.a:b.1"),
            };
            assert_eq!(Record::decode(&record.encode().unwrap()), Some(record));
        }
    }

    #[test]
    fn reclamation_accepts_only_absolute_hidden_siblings() {
        assert!(valid_record_paths(
            Path::new("/tmp/worktrees/repo/feature"),
            Path::new("/tmp/worktrees/repo/.perch-trash.feature.1")
        ));
        assert!(!valid_record_paths(
            Path::new("feature"),
            Path::new(".perch-trash.feature.1")
        ));
        assert!(!valid_record_paths(
            Path::new("/tmp/worktrees/repo/feature"),
            Path::new("/tmp/other/.perch-trash.feature.1")
        ));
        assert!(!valid_record_paths(
            Path::new("/tmp/worktrees/repo/feature"),
            Path::new("/tmp/worktrees/repo/feature")
        ));
    }

    #[test]
    fn filesystem_failures_are_reported_as_reclamation_errors() {
        let error = reclamation_error(
            "move worktree",
            Path::new("/tmp/worktrees/repo/feature"),
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );

        assert_eq!(
            error.to_string(),
            "worktree reclamation: could not move worktree: permission denied; files remain at /tmp/worktrees/repo/feature"
        );
    }
}

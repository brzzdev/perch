use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::{display_path, shell_quote};
use crate::grammar::Shell;
use crate::{AppResult, Error};

impl Shell {
    fn rc_file(self, home: &Path, shell_path: &Path) -> PathBuf {
        match self {
            Self::Bash => home.join(".bashrc"),
            Self::Fish => non_empty_var("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("fish/conf.d/perch.fish"),
            Self::Zsh => zsh_dotdir(shell_path)
                .or_else(|| non_empty_var("ZDOTDIR").map(PathBuf::from))
                .unwrap_or_else(|| home.to_path_buf())
                .join(".zshrc"),
        }
    }

    fn rc_line(self) -> &'static str {
        match self {
            Self::Bash => r#"eval "$(perch shell init bash)""#,
            Self::Fish => "perch shell init fish | source",
            Self::Zsh => r#"eval "$(perch shell init zsh)""#,
        }
    }

    pub(crate) fn script(self) -> &'static str {
        match self {
            Self::Bash | Self::Zsh => include_str!("../../shell/perch.sh"),
            Self::Fish => include_str!("../../shell/perch.fish"),
        }
    }
}

pub(crate) fn run_install() -> AppResult<()> {
    let (shell, shell_path) = login_shell()?;
    let home =
        non_empty_var("HOME").ok_or_else(|| Error::ShellInstall("$HOME is not set".into()))?;
    let home = Path::new(&home);
    let rc = shell.rc_file(home, &shell_path);
    let rc_display = display_path(&rc);
    let line = shell.rc_line();

    let appended = append_once(&rc, line)
        .map_err(|error| Error::ShellInstall(format!("{rc_display}: {error}")))?;
    if !appended {
        eprintln!("perch is already loaded from {rc_display}; nothing changed");
        return Ok(());
    }

    eprintln!("Added `{line}` to {rc_display}");
    eprintln!(
        "Open a new shell, or run `source {}`, to pick it up.",
        source_argument(&rc, home)
    );
    if shell == Shell::Bash && cfg!(target_os = "macos") {
        eprintln!(
            "macOS terminals start bash as a login shell, which reads ~/.bash_profile; \
             make sure it sources ~/.bashrc."
        );
    }
    Ok(())
}

/// Appends `line` unless the file already holds it, returning whether it did. Read as bytes, so an
/// rc file in a legacy encoding is still one perch can add to.
fn append_once(rc: &Path, line: &str) -> io::Result<bool> {
    let existing = match fs::read(rc) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if existing
        .split(|&byte| byte == b'\n')
        .any(|existing_line| existing_line.trim_ascii() == line.as_bytes())
    {
        return Ok(false);
    }

    if let Some(parent) = rc.parent() {
        fs::create_dir_all(parent)?;
    }
    let separator = if existing.is_empty() || existing.ends_with(b"\n") {
        ""
    } else {
        "\n"
    };
    let mut file = OpenOptions::new().create(true).append(true).open(rc)?;
    writeln!(file, "{separator}{line}")?;
    Ok(true)
}

/// `rc` spelled for pasting into a shell: quoted, with the home directory left as a bare `~` so it
/// still expands.
fn source_argument(rc: &Path, home: &Path) -> String {
    match rc.strip_prefix(home) {
        Ok(rest) => format!("~/{}", shell_quote(&rest.to_string_lossy())),
        Err(_) => shell_quote(&rc.to_string_lossy()),
    }
}

fn login_shell() -> AppResult<(Shell, PathBuf)> {
    let path = PathBuf::from(
        non_empty_var("SHELL").ok_or_else(|| Error::ShellInstall("$SHELL is not set".into()))?,
    );
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let shell =
        Shell::parse(name).map_err(|message| Error::ShellInstall(format!("$SHELL: {message}")))?;
    Ok((shell, path))
}

/// Asks zsh, because `.zshenv` can set `ZDOTDIR` without exporting it, which leaves it out of
/// perch's environment. The last line is the answer, since `.zshenv` may print before it.
fn zsh_dotdir(zsh: &Path) -> Option<PathBuf> {
    let output = Command::new(zsh)
        .args(["-c", "print -r -- ${ZDOTDIR:-$HOME}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let dir = stdout.lines().last()?;
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

fn non_empty_var(key: &str) -> Option<OsString> {
    env::var_os(key).filter(|value| !value.is_empty())
}

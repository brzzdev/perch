use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::app::display_path;
use crate::grammar::Shell;
use crate::{AppResult, Error};

impl Shell {
    fn script(self) -> &'static str {
        match self {
            Self::Bash | Self::Zsh => include_str!("../../shell/perch.sh"),
            Self::Fish => include_str!("../../shell/perch.fish"),
        }
    }

    fn rc_line(self) -> &'static str {
        match self {
            Self::Bash => r#"eval "$(perch shell init bash)""#,
            Self::Fish => "perch shell init fish | source",
            Self::Zsh => r#"eval "$(perch shell init zsh)""#,
        }
    }

    /// The file each shell reads at startup, wherever its own variables move it.
    fn rc_file(self, home: &Path) -> PathBuf {
        match self {
            Self::Bash => home.join(".bashrc"),
            Self::Fish => non_empty_var("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("fish/conf.d/perch.fish"),
            Self::Zsh => non_empty_var("ZDOTDIR")
                .map_or_else(|| home.to_path_buf(), PathBuf::from)
                .join(".zshrc"),
        }
    }
}

pub(crate) fn run_init(shell: Shell) {
    print!("{}", shell.script());
}

pub(crate) fn run_install() -> AppResult<()> {
    let shell = login_shell()?;
    let home =
        non_empty_var("HOME").ok_or_else(|| Error::ShellInstall("$HOME is not set".into()))?;
    let rc = shell.rc_file(Path::new(&home));
    let shown = display_path(&rc);
    let line = shell.rc_line();

    let existing = match fs::read_to_string(&rc) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if existing
        .lines()
        .any(|existing_line| existing_line.trim() == line)
    {
        eprintln!("perch is already loaded from {shown}; nothing changed");
        return Ok(());
    }

    if let Some(parent) = rc.parent() {
        fs::create_dir_all(parent)?;
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut file = OpenOptions::new().create(true).append(true).open(&rc)?;
    writeln!(file, "{separator}{line}")?;

    eprintln!("Added `{line}` to {shown}");
    eprintln!("Open a new shell, or run `source {shown}`, to pick it up.");
    Ok(())
}

fn login_shell() -> AppResult<Shell> {
    let path =
        non_empty_var("SHELL").ok_or_else(|| Error::ShellInstall("$SHELL is not set".into()))?;
    let name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Shell::from_name(&name).ok_or_else(|| {
        Error::ShellInstall(format!(
            "unsupported shell '{name}' (from $SHELL); perch supports {}",
            Shell::SUPPORTED
        ))
    })
}

fn non_empty_var(key: &str) -> Option<OsString> {
    env::var_os(key).filter(|value| !value.is_empty())
}

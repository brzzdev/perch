use std::fmt;
use std::io::ErrorKind;
use std::process::{Command, Stdio};

use crate::{AppResult, Error, git};

/// The remote whose forge `pr:<n>` and `mr:<n>` are looked up on.
const REMOTE: &str = "origin";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Forge {
    GitHub,
    GitLab,
}

impl Forge {
    fn cli(self) -> &'static str {
        match self {
            Self::GitHub => "gh",
            Self::GitLab => "glab",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::GitHub => "pr",
            Self::GitLab => "mr",
        }
    }

    /// The JSON flags and a `--jq` query printing `<from a fork> <head branch>`,
    /// the fork flag as `true` or `false`. A branch name holds no space, so one
    /// split reads both.
    fn view_query(self) -> ([&'static str; 2], &'static str) {
        match self {
            Self::GitHub => (
                ["--json", "isCrossRepository,headRefName"],
                r#""\(.isCrossRepository) \(.headRefName)""#,
            ),
            Self::GitLab => (
                ["--output", "json"],
                r#""\(.source_project_id != .target_project_id) \(.source_branch)""#,
            ),
        }
    }
}

/// A pull or merge request named by number, as `pr:<n>` or `mr:<n>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reference {
    forge: Forge,
    number: u64,
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.forge.prefix(), self.number)
    }
}

/// Why a request could not be followed to its head branch.
#[derive(Debug, thiserror::Error)]
pub enum Problem {
    #[error("needs `{0}`")]
    CliMissing(&'static str),

    #[error("failed: {0}")]
    Failed(String),

    #[error("comes from a fork, which perch does not check out yet")]
    Fork,

    #[error(
        "has head branch {branch}, which is not on {remote}; it may have been deleted on merge"
    )]
    HeadMissing { branch: String, remote: String },
}

impl Reference {
    /// `None` for an ordinary branch name. Git refuses `:` in a ref, so these
    /// prefixes never shadow a branch.
    pub(crate) fn parse(argument: &str) -> AppResult<Option<Self>> {
        let (forge, number) = if let Some(number) = argument.strip_prefix("pr:") {
            (Forge::GitHub, number)
        } else if let Some(number) = argument.strip_prefix("mr:") {
            (Forge::GitLab, number)
        } else {
            return Ok(None);
        };
        let number = number
            .parse()
            .map_err(|_| Error::InvalidForgeReference(argument.to_string()))?;
        Ok(Some(Self { forge, number }))
    }

    pub(crate) fn error(self, problem: Problem) -> Error {
        Error::Forge {
            reference: self,
            problem,
        }
    }

    /// The request's head branch on `origin`, asked of the forge's own CLI.
    pub(crate) fn head_branch(self) -> AppResult<String> {
        let url = git::remote_url(None, REMOTE).ok_or_else(|| {
            self.error(Problem::Failed(format!(
                "no `{REMOTE}` remote to look it up on"
            )))
        })?;
        let cli = self.forge.cli();
        let (json, query) = self.forge.view_query();
        let output = match Command::new(cli)
            .args([self.forge.prefix(), "view"])
            .arg(self.number.to_string())
            .args(["--repo", &url])
            .args(json)
            .args(["--jq", query])
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => output,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(self.error(Problem::CliMissing(cli)));
            }
            Err(e) => return Err(e.into()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(self.error(Problem::Failed(message.to_string())));
        }
        match stdout.trim().split_once(' ') {
            Some(("false", branch)) if !branch.is_empty() => Ok(branch.to_string()),
            Some(("true", _)) => Err(self.error(Problem::Fork)),
            _ => Err(self.error(Problem::Failed(format!(
                "unexpected answer from `{cli}`: {}",
                stdout.trim()
            )))),
        }
    }

    /// Refuses a head branch that is neither local nor on `remote`, such as one
    /// deleted when its request merged.
    pub(crate) fn ensure_head_exists(self, branch: &str, remote: &str) -> AppResult<()> {
        if git::resolve(None, &format!("refs/heads/{branch}")).is_some()
            || git::remote_branch_exists(remote, branch)
        {
            return Ok(());
        }
        Err(self.error(Problem::HeadMissing {
            branch: branch.to_string(),
            remote: remote.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_numbered_prefix_is_a_reference() {
        assert_eq!(
            Reference::parse("pr:12").unwrap(),
            Some(Reference {
                forge: Forge::GitHub,
                number: 12
            })
        );
        assert_eq!(
            Reference::parse("mr:3").unwrap().map(|r| r.to_string()),
            Some("mr:3".to_string())
        );
        assert_eq!(Reference::parse("feature/pr").unwrap(), None);
        assert_eq!(Reference::parse("PR:1").unwrap(), None);
        assert!(Reference::parse("pr:").is_err());
        assert!(Reference::parse("pr:12a").is_err());
    }
}

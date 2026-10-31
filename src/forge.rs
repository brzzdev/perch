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

    /// Arguments printing `<from a fork> <head branch>`, the fork flag as
    /// `true` or `false`. A branch name holds no space, so one split reads both.
    fn view_args(self, number: u64, url: &str) -> Vec<String> {
        let (subcommand, query) = match self {
            Self::GitHub => ("pr", r#""\(.isCrossRepository) \(.headRefName)""#),
            Self::GitLab => (
                "mr",
                r#""\(.source_project_id != .target_project_id) \(.source_branch)""#,
            ),
        };
        let json = match self {
            Self::GitHub => ["--json", "isCrossRepository,headRefName"],
            Self::GitLab => ["--output", "json"],
        };
        [subcommand, "view", &number.to_string(), "--repo", url]
            .into_iter()
            .chain(json)
            .chain(["--jq", query])
            .map(str::to_string)
            .collect()
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

    pub(crate) fn cli(self) -> &'static str {
        self.forge.cli()
    }

    /// The request's head branch on `origin`, asked of the forge's own CLI.
    pub(crate) fn head_branch(self) -> AppResult<String> {
        let url = git::remote_url(None, REMOTE).ok_or_else(|| Error::Forge {
            reference: self,
            message: format!("no `{REMOTE}` remote to look it up on"),
        })?;
        let output = match Command::new(self.cli())
            .args(self.forge.view_args(self.number, &url))
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => output,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::ForgeCliMissing { reference: self });
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
            return Err(Error::Forge {
                reference: self,
                message: message.to_string(),
            });
        }
        match stdout.trim().split_once(' ') {
            Some(("false", branch)) if !branch.is_empty() => Ok(branch.to_string()),
            Some(("true", _)) => Err(Error::ForkRequest { reference: self }),
            _ => Err(Error::Forge {
                reference: self,
                message: format!("unexpected answer from `{}`: {}", self.cli(), stdout.trim()),
            }),
        }
    }
}

/// Refuses a head branch that is neither local nor on `origin`, such as one
/// deleted when its request merged: navigating to it would make a new branch.
pub(crate) fn ensure_head_exists(reference: Reference, branch: &str) -> AppResult<()> {
    if git::local_branches()?.iter().any(|local| local == branch)
        || git::remote_branch_exists(REMOTE, branch)
    {
        return Ok(());
    }
    Err(Error::ForgeHeadMissing {
        reference,
        branch: branch.to_string(),
    })
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

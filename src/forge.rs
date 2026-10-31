use std::fmt;
use std::io::ErrorKind;
use std::process::{Command, Stdio};

use crate::{AppResult, Error, git};

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

    /// Flags printing `<true|false: from a fork> <head branch>`; a branch holds no space.
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
        let Some((forge, number)) = [Forge::GitHub, Forge::GitLab]
            .into_iter()
            .find_map(|forge| {
                let number = argument.strip_prefix(forge.prefix())?.strip_prefix(':')?;
                Some((forge, number))
            })
        else {
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
        let url = git::remote_url(None, git::DEFAULT_REMOTE).ok_or_else(|| {
            self.error(Problem::Failed(format!(
                "no `{}` remote to look it up on",
                git::DEFAULT_REMOTE
            )))
        })?;
        let cli = self.forge.cli();
        let (json, query) = self.forge.view_query();
        let output = match Command::new(cli)
            .args([self.forge.prefix(), "view"])
            .arg(self.number.to_string())
            .args(["--repo", &forge_url(&url)])
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
    /// deleted when its request merged. Cached refs failing, the server is asked.
    pub(crate) fn ensure_head_exists(self, branch: &str, remote: &str) -> AppResult<()> {
        if git::resolve(None, &format!("refs/heads/{branch}")).is_some()
            || git::remote_branch_exists(remote, branch)
        {
            return Ok(());
        }
        match git::server_has_branch(remote, branch) {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.error(Problem::HeadMissing {
                branch: branch.to_string(),
                remote: remote.to_string(),
            })),
            Err(e) => Err(self.error(Problem::Failed(e.to_string()))),
        }
    }
}

/// `url` as `gh` and `glab` read it. They take an SSH remote's host literally,
/// so one written through an `~/.ssh/config` alias becomes `https://` on the
/// host the alias stands for.
fn forge_url(url: &str) -> String {
    let Some((host, path)) = ssh_parts(url) else {
        return url.to_string();
    };
    format!("https://{}/{path}", aliased_host(host))
}

/// The host `ssh -G` connects to for `host`.
fn aliased_host(host: &str) -> String {
    let resolved = Command::new("ssh")
        .args(["-G", host])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("hostname "))
                .map(str::to_string)
        });
    forge_host(host, resolved.as_deref()).to_string()
}

/// `resolved` unless it is a subdomain of `host`: `altssh.gitlab.com` and
/// `ssh.github.com` serve SSH on port 443 for the forge `host` already names.
fn forge_host<'a>(host: &'a str, resolved: Option<&'a str>) -> &'a str {
    match resolved {
        Some(resolved) if !resolved.ends_with(&format!(".{host}")) => resolved,
        _ => host,
    }
}

/// The host and repository path of an SSH remote URL, scp-style or `ssh://`.
fn ssh_parts(url: &str) -> Option<(&str, &str)> {
    let (authority, path) = match url.split_once("://") {
        Some(("ssh" | "git+ssh" | "ssh+git", rest)) => rest.split_once('/')?,
        Some(_) => return None,
        // scp-style, `[user@]host:path`; a `/` before the `:` makes it a path.
        None => match url.find([':', '/']) {
            Some(colon) if url[colon..].starts_with(':') => (&url[..colon], &url[colon + 1..]),
            _ => return None,
        },
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _)| host);
    Some((host, path.trim_start_matches('/')))
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

    #[test]
    fn only_an_ssh_remote_has_a_host_and_path_to_rewrite() {
        assert_eq!(
            ssh_parts("git@github-work:o/r.git"),
            Some(("github-work", "o/r.git"))
        );
        assert_eq!(
            ssh_parts("github-work:o/r.git"),
            Some(("github-work", "o/r.git"))
        );
        assert_eq!(
            ssh_parts("ssh://git@gitlab-work:2222/group/sub/r.git"),
            Some(("gitlab-work", "group/sub/r.git"))
        );
        assert_eq!(
            ssh_parts("git+ssh://gitlab-work//o/r"),
            Some(("gitlab-work", "o/r"))
        );
        assert_eq!(ssh_parts("https://github.com/o/r.git"), None);
        assert_eq!(ssh_parts("/srv/git/r.git"), None);
        assert_eq!(ssh_parts("./a:b"), None);
    }

    #[test]
    fn an_alias_resolves_but_a_port_443_endpoint_keeps_the_forge_host() {
        assert_eq!(forge_host("github-work", Some("github.com")), "github.com");
        assert_eq!(
            forge_host("github.com-work", Some("github.com")),
            "github.com"
        );
        assert_eq!(
            forge_host("gitlab.com", Some("altssh.gitlab.com")),
            "gitlab.com"
        );
        assert_eq!(
            forge_host("github.com", Some("ssh.github.com")),
            "github.com"
        );
        assert_eq!(forge_host("github.com", None), "github.com");
    }
}

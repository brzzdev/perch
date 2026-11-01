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
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| number.parse().ok())
            .flatten()
            .ok_or_else(|| Error::InvalidForgeReference(argument.to_string()))?;
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
            // `glab` frames its error in a box of padding lines; the message is last.
            let message = [&*stderr, &*stdout]
                .into_iter()
                .find_map(|text| text.lines().map(str::trim).rfind(|line| !line.is_empty()))
                .unwrap_or_default();
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
    let Some(parts) = ssh_parts(url) else {
        return url.to_string();
    };
    format!("https://{}/{}", aliased_host(&parts), parts.path)
}

/// The host `ssh -G` connects to for the remote, asked as its user, which a
/// `Match user` block can turn on.
fn aliased_host(parts: &SshParts<'_>) -> String {
    let destination = match parts.user {
        Some(user) => format!("{user}@{}", parts.host),
        None => parts.host.to_string(),
    };
    let resolved = Command::new("ssh")
        .args(["-G", &destination])
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
    forge_host(parts.host, resolved.as_deref()).to_string()
}

/// The forge host behind `host`, as `ssh -G` resolved it. A resolved subdomain
/// of `host` is an SSH endpoint for the forge `host` already names, and GitLab's
/// port-443 one, `altssh.gitlab.com`, answers no API under any alias.
fn forge_host<'a>(host: &'a str, resolved: Option<&'a str>) -> &'a str {
    let host = match resolved {
        Some(resolved) if !resolved.ends_with(&format!(".{host}")) => resolved,
        _ => host,
    };
    host.strip_prefix(GITLAB_PORT_443_PREFIX).unwrap_or(host)
}

/// The subdomain GitLab serves SSH from on port 443.
const GITLAB_PORT_443_PREFIX: &str = "altssh.";

/// An SSH remote URL's parts, scp-style or `ssh://`.
#[derive(Debug, PartialEq)]
struct SshParts<'a> {
    host: &'a str,
    path: &'a str,
    user: Option<&'a str>,
}

fn ssh_parts(url: &str) -> Option<SshParts<'_>> {
    let (authority, path) = match url.split_once("://") {
        Some(("ssh" | "git+ssh" | "ssh+git", rest)) => rest.split_once('/')?,
        Some(_) => return None,
        // scp-style, `[user@]host:path`; a `/` before the `:` makes it a path.
        None => match url.find([':', '/']) {
            Some(colon) if url[colon..].starts_with(':') => (&url[..colon], &url[colon + 1..]),
            _ => return None,
        },
    };
    let (user, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    Some(SshParts {
        host: host.split_once(':').map_or(host, |(host, _)| host),
        path: path.trim_start_matches('/'),
        user,
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
        assert!(Reference::parse("pr:+7").is_err());
    }

    #[test]
    fn only_an_ssh_remote_has_parts_to_rewrite() {
        let parts = |host, path, user| Some(SshParts { host, path, user });
        assert_eq!(
            ssh_parts("git@github-work:o/r.git"),
            parts("github-work", "o/r.git", Some("git"))
        );
        assert_eq!(
            ssh_parts("github-work:o/r.git"),
            parts("github-work", "o/r.git", None)
        );
        assert_eq!(
            ssh_parts("ssh://git@gitlab-work:2222/group/sub/r.git"),
            parts("gitlab-work", "group/sub/r.git", Some("git"))
        );
        assert_eq!(
            ssh_parts("git+ssh://gitlab-work//o/r"),
            parts("gitlab-work", "o/r", None)
        );
        assert_eq!(ssh_parts("https://github.com/o/r.git"), None);
        assert_eq!(ssh_parts("/srv/git/r.git"), None);
        assert_eq!(ssh_parts("./a:b"), None);
    }

    #[test]
    fn an_alias_resolves_to_the_forge_its_port_443_endpoint_serves() {
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
            forge_host("gitlab-work", Some("altssh.gitlab.com")),
            "gitlab.com"
        );
        assert_eq!(forge_host("altssh.gitlab.com", None), "gitlab.com");
        assert_eq!(
            forge_host("github.com", Some("ssh.github.com")),
            "github.com"
        );
        assert_eq!(
            forge_host("gitlab.corp", Some("ssh.gitlab.corp")),
            "gitlab.corp"
        );
        assert_eq!(forge_host("github.com", None), "github.com");
    }
}

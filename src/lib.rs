pub mod app;
pub mod git;
mod grammar;
mod session;

pub use grammar::GrammarError;

pub type AppResult<T> = Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git {command}: {message}")]
    Git { command: String, message: String },

    #[error("worktree reclamation: {0}")]
    Reclamation(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Grammar(#[from] GrammarError),

    #[error("not on a branch (detached HEAD); nothing to refresh")]
    Detached,

    #[error("branch diverged from remote")]
    Diverged,

    /// `br` was asked to check out a branch git already holds in another
    /// worktree. The message names the path and the verb that does reach it,
    /// because that hand-off is the whole difference between the two verbs.
    #[error(
        "{branch} is checked out at {path}; run `perch {}` to go there",
        app::go_there_argument(branch)
    )]
    HeldByWorktree { branch: String, path: String },

    #[error("{branch} is checked out at {path}; {hint}")]
    HeldForRemoval {
        branch: String,
        path: String,
        hint: String,
    },

    #[error("branch '{branch}' does not exist locally")]
    LocalBranchNotFound { branch: String },

    #[error("no branches found")]
    NoBranches,

    #[error("{branch} has no removable upstream: {reason}")]
    NoRemovableUpstream { branch: String, reason: String },

    #[error("invalid number from git: {0}")]
    ParseInt(#[from] std::num::ParseIntError),

    #[error("one or more requested removals failed")]
    RemovalFailed,

    /// A destructive action was declined because its risk could not be shown:
    /// there was no terminal to warn on or ask in, and `--force` wasn't given.
    #[error("{0}")]
    Unconfirmed(String),

    /// A branch picked from a row went away between the list being drawn and
    /// the selection being resolved. Its own error because the alternative is
    /// silent: with nothing left to find, the name would otherwise read as a
    /// branch to create, and the user would be handed a fresh branch off the
    /// default in place of the one they chose.
    #[error("{branch} no longer exists; it was deleted after the list was drawn")]
    Vanished { branch: String },
}

impl Error {
    /// True when this wraps a Ctrl+C (`Interrupted`) raised by an interactive
    /// prompt. Callers that otherwise swallow prompt errors must still honour it
    /// so the user can cancel — raw mode delivers Ctrl+C as this error rather
    /// than a SIGINT.
    #[must_use]
    pub fn is_interrupt(&self) -> bool {
        matches!(self, Error::Io(io) if io.kind() == std::io::ErrorKind::Interrupted)
    }
}

pub fn run(args: &[String]) -> AppResult<()> {
    app::run_invocation(grammar::parse(args)?)
}

use std::path::PathBuf;

/// An [`Invocation`] and the global options that set up the run around it.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CommandLine {
    /// Each `-C` path in order, to change directory to one after another.
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) hook_environment: Vec<HookVariable>,
    pub(crate) invocation: Invocation,
}

/// One `--hook-env KEY=VALUE` pair.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct HookVariable {
    key: String,
    value: String,
}

/// The prefix every `--hook-env` key carries, so a caller can name its own
/// variables but never reach `PATH`, `BASH_ENV`, or anything else a hook's
/// shell reads.
const HOOK_VARIABLE_PREFIX: &str = "PERCH_";

impl HookVariable {
    pub(crate) fn parse(pair: &str) -> Result<Self, GrammarError> {
        match pair.split_once('=') {
            Some(("", _)) | None => Err(GrammarError::invalid_hook_variable(pair)),
            Some((key, _)) if !key.starts_with(HOOK_VARIABLE_PREFIX) => {
                Err(GrammarError::disallowed_hook_variable(key))
            }
            Some((key, value)) => Ok(Self {
                key: key.to_string(),
                value: value.to_string(),
            }),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Invocation {
    Complete(Completion),
    Help(HelpPage),
    InitShell(Shell),
    InstallShell,
    ListWorktrees,
    Navigate(Navigation),
    RemoveBranches(BranchRemoval),
    RemoveWorktrees(WorktreeRemoval),
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Navigation {
    Go(Option<String>),
    Here(Option<String>),
    Worktree {
        worktree_name: Option<WorktreeDirectoryName>,
        target: Option<String>,
        shell_handoff: ShellHandoff,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorktreeDirectoryName(String);

impl WorktreeDirectoryName {
    fn parse(name: &str) -> Result<Self, GrammarError> {
        if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
            return Err(GrammarError::worktree_navigation(format!(
                "invalid worktree directory name '{name}'; expected one directory name"
            )));
        }
        Ok(Self(name.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verb {
    Go,
    Here,
    Worktree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellHandoff {
    Emit,
    Suppress,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BranchRemoval {
    target: Option<String>,
    force: bool,
    upstream: bool,
}

impl BranchRemoval {
    pub(crate) fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub(crate) fn force(&self) -> bool {
        self.force
    }

    pub(crate) fn upstream(&self) -> bool {
        self.upstream
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorktreeRemoval {
    target: Option<String>,
    force: bool,
}

impl WorktreeRemoval {
    pub(crate) fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub(crate) fn force(&self) -> bool {
        self.force
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Shell {
    Bash,
    Fish,
    Zsh,
}

impl Shell {
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        match name {
            "bash" => Ok(Self::Bash),
            "fish" => Ok(Self::Fish),
            "zsh" => Ok(Self::Zsh),
            _ => Err(format!(
                "unsupported shell '{name}'; expected bash, fish or zsh"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HelpPage {
    Branch,
    Main,
    Shell,
    Worktree,
}

impl HelpPage {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Branch => BRANCH_HELP,
            Self::Main => MAIN_HELP,
            Self::Shell => SHELL_HELP,
            Self::Worktree => WORKTREE_HELP,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionSource {
    LocalBranches,
    ReachableBranches,
    Worktrees,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Position {
    Bare,
    Branch,
    Removal,
    Unfiltered,
    Worktree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Escape {
    Bare,
    Escaped,
}

impl Escape {
    fn completion_position(self) -> Position {
        match self {
            Self::Bare => Position::Worktree,
            Self::Escaped => Position::Unfiltered,
        }
    }
}

impl Position {
    fn eats(self, word: &str) -> bool {
        match self {
            Self::Bare => needs_top_level_escape(word),
            Self::Branch => parse_branch_subverb(word).is_some(),
            Self::Removal | Self::Unfiltered => false,
            Self::Worktree => parse_worktree_subverb(word).is_some(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Completion {
    source: CompletionSource,
    position: Position,
}

impl Completion {
    pub(crate) fn source(&self) -> CompletionSource {
        self.source
    }

    pub(crate) fn render<'a>(&self, candidates: impl IntoIterator<Item = &'a str>) -> String {
        let mut output = String::new();
        for candidate in candidates {
            if !self.position.eats(candidate) {
                output.push_str(candidate);
                output.push('\n');
            }
        }
        output
    }
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
#[error(transparent)]
pub struct GrammarError(GrammarErrorKind);

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
enum GrammarErrorKind {
    #[error("invalid `perch br` invocation: {0}")]
    BranchNavigation(String),

    #[error("invalid `perch br rm` invocation: {0}")]
    BranchRemoval(String),

    #[error("`--hook-env` key '{0}' is not allowed; keys must start with {HOOK_VARIABLE_PREFIX}")]
    DisallowedHookVariable(String),

    #[error("invalid `--hook-env` pair '{0}'; expected KEY=VALUE with a non-empty KEY")]
    InvalidHookVariable(String),

    #[error("`{0}` needs a value")]
    MissingValue(&'static str),

    #[error("invalid `perch` invocation: {0}")]
    Navigation(String),

    #[error("`--no-switch` does not apply to `perch wt {subverb}`")]
    NoSwitchWithSubverb { subverb: String },

    #[error(
        "`perch wt {word}` is gone; use `perch wt {keep}`, or `perch wt -- {word}` for a branch by that name"
    )]
    Retired {
        word: &'static str,
        keep: &'static str,
    },

    #[error("invalid `perch shell` invocation: {0}")]
    Shell(String),

    #[error("invalid `perch wt new` invocation: {0}")]
    WorktreeCreation(String),

    #[error("invalid `perch wt rm` invocation: {0}")]
    WorktreeRemoval(String),

    #[error("invalid `perch wt` invocation: {0}")]
    WorktreeNavigation(String),
}

impl GrammarError {
    fn branch_navigation(message: String) -> Self {
        Self(GrammarErrorKind::BranchNavigation(message))
    }

    fn branch_removal(message: String) -> Self {
        Self(GrammarErrorKind::BranchRemoval(message))
    }

    fn disallowed_hook_variable(key: &str) -> Self {
        Self(GrammarErrorKind::DisallowedHookVariable(key.to_string()))
    }

    fn invalid_hook_variable(pair: &str) -> Self {
        Self(GrammarErrorKind::InvalidHookVariable(pair.to_string()))
    }

    fn missing_value(option: &'static str) -> Self {
        Self(GrammarErrorKind::MissingValue(option))
    }

    fn navigation(message: String) -> Self {
        Self(GrammarErrorKind::Navigation(message))
    }

    fn shell(message: String) -> Self {
        Self(GrammarErrorKind::Shell(message))
    }

    fn worktree_creation(message: String) -> Self {
        Self(GrammarErrorKind::WorktreeCreation(message))
    }

    fn worktree_removal(message: String) -> Self {
        Self(GrammarErrorKind::WorktreeRemoval(message))
    }

    fn worktree_navigation(message: String) -> Self {
        Self(GrammarErrorKind::WorktreeNavigation(message))
    }

    fn retired(word: &'static str, keep: &'static str) -> Self {
        Self(GrammarErrorKind::Retired { word, keep })
    }

    fn no_switch_with_subverb(subverb: String) -> Self {
        Self(GrammarErrorKind::NoSwitchWithSubverb { subverb })
    }
}

#[derive(Clone, Copy)]
enum WorktreeSubverb {
    List,
    Ls,
    New,
    Remove,
    Rm,
}

const DIRECTORY_OPTION: &str = "-C";
const HOOK_ENVIRONMENT_OPTION: &str = "--hook-env";

/// `-C` and `--hook-env`.
#[derive(Default)]
struct RunOptions {
    directories: Vec<PathBuf>,
    hook_environment: Vec<HookVariable>,
}

impl RunOptions {
    /// Reads options in any order, returning the words from the first one that
    /// isn't an option.
    fn read<'a>(&mut self, mut rest: &'a [String]) -> Result<&'a [String], GrammarError> {
        loop {
            rest = match rest {
                [option, path, tail @ ..] if option == DIRECTORY_OPTION => {
                    // As in git, an empty path leaves the directory unchanged.
                    if !path.is_empty() {
                        self.directories.push(PathBuf::from(path));
                    }
                    tail
                }
                [option, pair, tail @ ..] if option == HOOK_ENVIRONMENT_OPTION => {
                    self.hook_environment.push(HookVariable::parse(pair)?);
                    tail
                }
                [option] if option == DIRECTORY_OPTION => {
                    return Err(GrammarError::missing_value(DIRECTORY_OPTION));
                }
                [option] if option == HOOK_ENVIRONMENT_OPTION => {
                    return Err(GrammarError::missing_value(HOOK_ENVIRONMENT_OPTION));
                }
                _ => return Ok(rest),
            };
        }
    }
}

/// Reads the global options ahead of the command word, in any order, then
/// [`parse`]s the rest.
pub(crate) fn parse_command_line(args: &[String]) -> Result<CommandLine, GrammarError> {
    let mut options = RunOptions::default();
    let rest = options.read(args)?;
    let invocation = parse(rest, &mut options)?;
    Ok(CommandLine {
        directories: options.directories,
        hook_environment: options.hook_environment,
        invocation,
    })
}

fn parse(args: &[String], options: &mut RunOptions) -> Result<Invocation, GrammarError> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => Ok(Invocation::Help(HelpPage::Main)),
        Some("--version" | "-V") => Ok(Invocation::Version),
        Some("--complete") => Ok(branch_completion(Position::Bare)),
        Some("--") => {
            reject_extra(args, 2, GrammarError::navigation)?;
            Ok(parse_escaped(args.get(1), Verb::Go))
        }
        Some(word) => match parse_command_word(word) {
            Some(CommandWord::Branch) => parse_branch(&args[1..]),
            Some(CommandWord::Shell) => parse_shell(&args[1..]),
            Some(CommandWord::Worktree) => parse_worktree(&args[1..], options),
            None => {
                reject_extra(args, 1, GrammarError::navigation)?;
                Ok(navigate(Verb::Go, Some(word)))
            }
        },
        None => Ok(navigate(Verb::Go, None)),
    }
}

pub(crate) fn needs_top_level_escape(word: &str) -> bool {
    parse_command_word(word).is_some()
}

fn parse_shell(args: &[String]) -> Result<Invocation, GrammarError> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--help" | "-h"] => Ok(Invocation::Help(HelpPage::Shell)),
        ["init", name] => Shell::parse(name)
            .map(Invocation::InitShell)
            .map_err(GrammarError::shell),
        ["install"] => Ok(Invocation::InstallShell),
        _ => Err(GrammarError::shell(
            "expected `init <shell>` or `install`".to_string(),
        )),
    }
}

fn parse_branch(args: &[String]) -> Result<Invocation, GrammarError> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => Ok(Invocation::Help(HelpPage::Branch)),
        Some("--complete") => Ok(branch_completion(Position::Branch)),
        Some("--") => {
            reject_extra(args, 2, GrammarError::branch_navigation)?;
            Ok(parse_escaped(args.get(1), Verb::Here))
        }
        Some(word) if parse_branch_subverb(word).is_some() => parse_branch_removal(&args[1..]),
        Some(word) => {
            reject_extra(args, 1, GrammarError::branch_navigation)?;
            Ok(navigate(Verb::Here, Some(word)))
        }
        None => Ok(navigate(Verb::Here, None)),
    }
}

fn parse_branch_removal(args: &[String]) -> Result<Invocation, GrammarError> {
    if args == ["--complete"] {
        return Ok(Invocation::Complete(Completion {
            source: CompletionSource::LocalBranches,
            position: Position::Removal,
        }));
    }

    let mut removal = BranchRemoval::default();
    for arg in args {
        match arg.as_str() {
            "--force" if !removal.force => removal.force = true,
            "--upstream" if !removal.upstream => removal.upstream = true,
            "--force" | "--upstream" => {
                return Err(GrammarError::branch_removal(format!(
                    "duplicate option '{arg}'"
                )));
            }
            _ if arg.starts_with('-') => {
                return Err(GrammarError::branch_removal(format!(
                    "unknown option '{arg}'"
                )));
            }
            _ if removal.target.is_some() => {
                return Err(GrammarError::branch_removal(format!(
                    "unexpected extra target '{arg}'"
                )));
            }
            _ => removal.target = Some(arg.clone()),
        }
    }
    Ok(Invocation::RemoveBranches(removal))
}

fn parse_worktree(args: &[String], options: &mut RunOptions) -> Result<Invocation, GrammarError> {
    let leading_no_switches = args.iter().take_while(|arg| *arg == "--no-switch").count();
    if let Some(word) = args.get(leading_no_switches)
        && let Some(subverb) = parse_worktree_subverb(word)
    {
        if leading_no_switches > 0 {
            return Err(GrammarError::no_switch_with_subverb(word.clone()));
        }
        let rest = &args[1..];
        return match subverb {
            WorktreeSubverb::List => Err(GrammarError::retired("list", "ls")),
            WorktreeSubverb::Ls => Ok(Invocation::ListWorktrees),
            WorktreeSubverb::New => parse_worktree_creation(rest, options),
            WorktreeSubverb::Remove => Err(GrammarError::retired("remove", "rm")),
            WorktreeSubverb::Rm => parse_worktree_removal(rest),
        };
    }

    let mut shell_handoff = ShellHandoff::Emit;
    let mut reads_options = true;
    let mut remaining = Vec::with_capacity(args.len());
    for arg in args {
        if reads_options && arg == "--" {
            reads_options = false;
            remaining.push(arg);
        } else if reads_options && arg == "--no-switch" {
            shell_handoff = ShellHandoff::Suppress;
        } else {
            remaining.push(arg);
        }
    }

    match remaining.first().map(|arg| arg.as_str()) {
        Some("--help" | "-h") => Ok(Invocation::Help(HelpPage::Worktree)),
        Some("--complete") => Ok(branch_completion(Position::Worktree)),
        Some("--") => parse_worktree_navigation(&remaining[1..], shell_handoff, Escape::Escaped),
        Some(option) if option.starts_with('-') => Err(GrammarError::worktree_navigation(format!(
            "unknown option '{option}'"
        ))),
        Some(_) => parse_worktree_navigation(&remaining, shell_handoff, Escape::Bare),
        None => Ok(worktree_navigation(None, None, shell_handoff)),
    }
}

fn parse_worktree_navigation(
    args: &[&String],
    shell_handoff: ShellHandoff,
    escape: Escape,
) -> Result<Invocation, GrammarError> {
    match args {
        [] => Ok(worktree_navigation(None, None, shell_handoff)),
        [target] if target.as_str() == "--complete" => {
            Ok(branch_completion(escape.completion_position()))
        }
        [target] => Ok(worktree_navigation(None, Some(target), shell_handoff)),
        [worktree_name, target] if target.as_str() == "--complete" => {
            WorktreeDirectoryName::parse(worktree_name)?;
            Ok(branch_completion(Position::Unfiltered))
        }
        [_, option] if escape == Escape::Bare && option.starts_with('-') => Err(
            GrammarError::worktree_navigation(format!("unknown option '{option}'")),
        ),
        // COMPAT: Before the two-argument form, `--` consumed only the next word.
        // Preserve the shell handoff for `wt -- <branch> --no-switch`.
        [branch, ignored_no_switch]
            if escape == Escape::Escaped && ignored_no_switch.as_str() == "--no-switch" =>
        {
            Ok(worktree_navigation(None, Some(branch), shell_handoff))
        }
        [worktree_name, target] => {
            let worktree_name = WorktreeDirectoryName::parse(worktree_name)?;
            Ok(worktree_navigation(
                Some(worktree_name),
                Some(target),
                shell_handoff,
            ))
        }
        [_, _, extra, ..] => Err(GrammarError::worktree_navigation(format!(
            "unexpected extra argument '{extra}'"
        ))),
    }
}

/// `wt new`: creation alone, so no word after it is a *Subverb*, and a target
/// is never taken for an option.
fn parse_worktree_creation(
    args: &[String],
    options: &mut RunOptions,
) -> Result<Invocation, GrammarError> {
    let rest = options.read(args)?;
    let positionals = match rest {
        [escape, positionals @ ..] if escape.as_str() == "--" => positionals,
        [option, ..] if option.starts_with('-') => {
            return Err(GrammarError::worktree_creation(format!(
                "unknown option '{option}'"
            )));
        }
        positionals => positionals,
    };
    let (worktree_name, target) = match positionals {
        [] => {
            return Err(GrammarError::worktree_creation(
                "missing branch, pr:<n>, or mr:<n>".to_string(),
            ));
        }
        [target] => (None, target),
        [worktree_name, target] => (Some(WorktreeDirectoryName::parse(worktree_name)?), target),
        [_, _, extra, ..] => {
            return Err(GrammarError::worktree_creation(format!(
                "unexpected extra argument '{extra}'"
            )));
        }
    };
    if target.starts_with('-') {
        return Err(GrammarError::worktree_creation(format!(
            "'{target}' is not a branch; no branch begins with '-'"
        )));
    }
    Ok(worktree_navigation(
        worktree_name,
        Some(target),
        ShellHandoff::Suppress,
    ))
}

fn parse_worktree_removal(args: &[String]) -> Result<Invocation, GrammarError> {
    for arg in args {
        if arg.as_str() == "--" {
            break;
        }
        if arg.as_str() == "--complete" {
            return Ok(Invocation::Complete(Completion {
                source: CompletionSource::Worktrees,
                position: Position::Removal,
            }));
        }
    }

    let mut removal = WorktreeRemoval::default();
    let mut reads_options = true;
    for arg in args {
        match arg.as_str() {
            "--" if reads_options => reads_options = false,
            "-f" | "--force" if reads_options && !removal.force => removal.force = true,
            "-f" | "--force" if reads_options => {
                return Err(GrammarError::worktree_removal(format!(
                    "duplicate option '{arg}'"
                )));
            }
            option if reads_options && option.starts_with('-') => {
                return Err(GrammarError::worktree_removal(format!(
                    "unknown option '{option}'"
                )));
            }
            target if removal.target.is_some() => {
                return Err(GrammarError::worktree_removal(format!(
                    "unexpected extra target '{target}'"
                )));
            }
            target => removal.target = Some(target.to_string()),
        }
    }
    Ok(Invocation::RemoveWorktrees(removal))
}

/// Refuses whatever follows the `taken` words that name a target, so a
/// misplaced option cannot pass silently.
fn reject_extra(
    args: &[String],
    taken: usize,
    error: fn(String) -> GrammarError,
) -> Result<(), GrammarError> {
    match args.get(taken) {
        Some(extra) => Err(error(format!("unexpected extra argument '{extra}'"))),
        None => Ok(()),
    }
}

fn parse_escaped(target: Option<&String>, verb: Verb) -> Invocation {
    if target.is_some_and(|word| word == "--complete") {
        branch_completion(Position::Unfiltered)
    } else {
        navigate(verb, target.map(String::as_str))
    }
}

fn navigate(verb: Verb, target: Option<&str>) -> Invocation {
    let target = target.map(str::to_string);
    Invocation::Navigate(match verb {
        Verb::Go => Navigation::Go(target),
        Verb::Here => Navigation::Here(target),
        Verb::Worktree => Navigation::Worktree {
            worktree_name: None,
            target,
            shell_handoff: ShellHandoff::Emit,
        },
    })
}

fn worktree_navigation(
    worktree_name: Option<WorktreeDirectoryName>,
    target: Option<&str>,
    shell_handoff: ShellHandoff,
) -> Invocation {
    Invocation::Navigate(Navigation::Worktree {
        worktree_name,
        target: target.map(str::to_string),
        shell_handoff,
    })
}

fn branch_completion(position: Position) -> Invocation {
    Invocation::Complete(Completion {
        source: CompletionSource::ReachableBranches,
        position,
    })
}

enum CommandWord {
    Branch,
    Shell,
    Worktree,
}

fn parse_command_word(word: &str) -> Option<CommandWord> {
    match word {
        "br" => Some(CommandWord::Branch),
        "shell" => Some(CommandWord::Shell),
        "wt" => Some(CommandWord::Worktree),
        _ => None,
    }
}

fn parse_branch_subverb(word: &str) -> Option<()> {
    (word == "rm").then_some(())
}

fn parse_worktree_subverb(word: &str) -> Option<WorktreeSubverb> {
    match word {
        "list" => Some(WorktreeSubverb::List),
        "ls" => Some(WorktreeSubverb::Ls),
        "new" => Some(WorktreeSubverb::New),
        "remove" => Some(WorktreeSubverb::Remove),
        "rm" => Some(WorktreeSubverb::Rm),
        _ => None,
    }
}

const MAIN_HELP: &str = concat!(
    "Usage: perch [<branch>|pr:<n>|mr:<n>]\n",
    "                                  Go to the branch, wherever it lives\n",
    "       perch br [<branch>|pr:<n>|mr:<n>]\n",
    "                                  Check the branch out here\n",
    "       perch wt [<branch>|pr:<n>|mr:<n>]\n",
    "                                  Give the branch its own worktree\n",
    "       perch wt <name> <branch>|pr:<n>|mr:<n>\n",
    "                                  Use a custom worktree directory name\n",
    "\n",
    "       perch .                Refresh the current branch from its remote\n",
    "       perch -- <branch>      Go to a branch named br/shell/wt\n",
    "       perch br rm [<branch>] Remove local branches\n",
    "       perch wt ls            List worktrees\n",
    "       perch wt new [<name>] <branch>|pr:<n>|mr:<n>\n",
    "                                  Create or find a worktree, never switching to it\n",
    "       perch wt rm [<branch>|.]\n",
    "       perch shell install    Add the shell integration to your rc file\n",
    "\n",
    "Options:\n",
    "  -C <path>              Run as if perch was started in <path>\n",
    "      --hook-env <PERCH_KEY=VALUE>\n",
    "                         Add PERCH_KEY=VALUE to every hook this run fires; repeatable\n",
    "\n",
    "With the shell integration loaded, `br` and `wt` stand in for `perch br`\n",
    "and `perch wt`. Set PERCH_NO_SHORTCUTS to leave both names alone.\n",
);

const BRANCH_HELP: &str = concat!(
    "Usage: perch br [<branch>|pr:<n>|mr:<n>]\n",
    "                                  Check the branch out here\n",
    "       perch br rm [<branch>] [--upstream] [--force]\n",
    "                                  Remove local branches\n",
    "       perch br -- <branch>   Check out a branch named rm\n",
    "\n",
    "Options:\n",
    "      --upstream  Also offer the branch's same-named upstream for removal\n",
    "      --force     Skip destructive confirmations\n",
);

const SHELL_HELP: &str = concat!(
    "Usage: perch shell init <bash|fish|zsh>\n",
    "                                  Print the shell integration script\n",
    "       perch shell install    Add the shell integration to your rc file\n",
    "\n",
    "install reads your shell from $SHELL and appends one line to ~/.bashrc,\n",
    "~/.config/fish/conf.d/perch.fish (under $XDG_CONFIG_HOME if set), or\n",
    "~/.zshrc (under $ZDOTDIR if set). Running it again changes nothing.\n",
);

const WORKTREE_HELP: &str = concat!(
    "Usage: perch wt [<branch>|pr:<n>|mr:<n>] [--no-switch]\n",
    "                                  Give the branch its own worktree\n",
    "       perch wt <worktree-directory-name> <branch>|pr:<n>|mr:<n> [--no-switch]\n",
    "                                  Use a custom worktree directory name\n",
    "       perch wt ls            List worktrees\n",
    "       perch wt new [-C <path>]... [--hook-env <PERCH_KEY=VALUE>]...\n",
    "                    [--] [<worktree-directory-name>] <branch>|pr:<n>|mr:<n>\n",
    "                                  Create or find a worktree, never switching to it\n",
    "       perch wt rm [<branch>] Remove a worktree (deletes branch if merged)\n",
    "       perch wt rm .          Remove the worktree you're in\n",
    "       perch wt -- <branch>   Worktree a branch named ls/new/rm/list/remove\n",
    "       perch wt -- <worktree-directory-name> <branch>\n",
    "                                  Escape a colliding worktree directory name\n",
    "\n",
    "Options:\n",
    "      --no-switch  Create or find the worktree without switching to it\n",
    "  -f, --force      Skip the confirmation for uncommitted or unmerged work\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    fn command_line(words: &[&str]) -> Result<CommandLine, GrammarError> {
        parse_command_line(&args(words))
    }

    fn parse(args: &[String]) -> Result<Invocation, GrammarError> {
        parse_command_line(args).map(|command_line| command_line.invocation)
    }

    #[test]
    fn each_directory_option_is_kept_in_order_and_an_empty_one_skipped() {
        let parsed = command_line(&["-C", "/repos", "-C", "", "-C", "perch", "wt", "ls"]).unwrap();

        assert_eq!(
            parsed.directories,
            [PathBuf::from("/repos"), PathBuf::from("perch")]
        );
        assert_eq!(parsed.invocation, Invocation::ListWorktrees);
    }

    #[test]
    fn a_word_after_a_navigation_target_is_refused() {
        for (words, message) in [
            (
                ["pr:77", "-C", "repo"].as_slice(),
                "invalid `perch` invocation: unexpected extra argument '-C'",
            ),
            (
                ["--", "br", "extra"].as_slice(),
                "invalid `perch` invocation: unexpected extra argument 'extra'",
            ),
            (
                ["br", "topic", "--hook-env", "PERCH_K=V"].as_slice(),
                "invalid `perch br` invocation: unexpected extra argument '--hook-env'",
            ),
            (
                ["br", "--", "rm", "extra"].as_slice(),
                "invalid `perch br` invocation: unexpected extra argument 'extra'",
            ),
        ] {
            assert_eq!(command_line(words).unwrap_err().to_string(), message);
        }
    }

    #[test]
    fn global_options_after_the_command_word_are_unknown_options() {
        let directory = command_line(&["wt", "-C", "repo"]).unwrap_err();
        assert_eq!(
            directory.to_string(),
            "invalid `perch wt` invocation: unknown option '-C'"
        );

        let hook_environment =
            command_line(&["wt", "rm", "topic", "--hook-env", "PERCH_K=V"]).unwrap_err();
        assert_eq!(
            hook_environment.to_string(),
            "invalid `perch wt rm` invocation: unknown option '--hook-env'"
        );
    }

    #[test]
    fn hook_env_pairs_mix_with_directory_options_ahead_of_the_command_word() {
        let parsed = command_line(&[
            "-C",
            "repo",
            "--hook-env",
            "PERCH_SESSION=one=two",
            "-C",
            "sub",
            "--hook-env",
            "PERCH_EMPTY=",
            "wt",
            "77",
            "#77",
            "--no-switch",
        ])
        .unwrap();

        assert_eq!(
            parsed.directories,
            [PathBuf::from("repo"), PathBuf::from("sub")]
        );
        let pairs: Vec<(&str, &str)> = parsed
            .hook_environment
            .iter()
            .map(|variable| (variable.key(), variable.value()))
            .collect();
        assert_eq!(pairs, [("PERCH_SESSION", "one=two"), ("PERCH_EMPTY", "")]);
        assert_eq!(
            parsed.invocation,
            Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("77".into())),
                target: Some("#77".into()),
                shell_handoff: ShellHandoff::Suppress,
            })
        );
    }

    #[test]
    fn wt_new_reads_run_options_after_the_global_ones_and_never_hands_off() {
        let parsed = command_line(&[
            "-C",
            "/repos",
            "--hook-env",
            "PERCH_ONE=1",
            "wt",
            "new",
            "--hook-env",
            "PERCH_REX_SESSION=a=b",
            "-C",
            "perch",
            "77",
            "pr:77",
        ])
        .unwrap();

        assert_eq!(
            parsed.directories,
            [PathBuf::from("/repos"), PathBuf::from("perch")]
        );
        let pairs: Vec<(&str, &str)> = parsed
            .hook_environment
            .iter()
            .map(|variable| (variable.key(), variable.value()))
            .collect();
        assert_eq!(pairs, [("PERCH_ONE", "1"), ("PERCH_REX_SESSION", "a=b")]);
        assert_eq!(
            parsed.invocation,
            Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("77".into())),
                target: Some("pr:77".into()),
                shell_handoff: ShellHandoff::Suppress,
            })
        );
    }

    #[test]
    fn wt_new_reads_no_subverb_after_it() {
        assert_eq!(
            parse(&args(&["wt", "new", "rm", "topic"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("rm".into())),
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Suppress,
            }))
        );
        assert_eq!(
            parse(&args(&["wt", "new", "--", "-f", "topic"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("-f".into())),
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Suppress,
            }))
        );
    }

    #[test]
    fn wt_new_refuses_what_it_does_not_define() {
        for (words, message) in [
            (
                ["wt", "new"].as_slice(),
                "invalid `perch wt new` invocation: missing branch, pr:<n>, or mr:<n>",
            ),
            (
                ["wt", "new", "x", "topic", "extra"].as_slice(),
                "invalid `perch wt new` invocation: unexpected extra argument 'extra'",
            ),
            (
                ["wt", "new", "-f", "topic"].as_slice(),
                "invalid `perch wt new` invocation: unknown option '-f'",
            ),
            (
                ["wt", "new", "x", "--complete"].as_slice(),
                "invalid `perch wt new` invocation: '--complete' is not a branch; no branch begins with '-'",
            ),
            (
                ["wt", "new", "x", "--no-switch"].as_slice(),
                "invalid `perch wt new` invocation: '--no-switch' is not a branch; no branch begins with '-'",
            ),
            (
                ["wt", "--no-switch", "new", "x", "topic"].as_slice(),
                "`--no-switch` does not apply to `perch wt new`",
            ),
            (
                ["wt", "new", "--", "x", "--no-switch"].as_slice(),
                "invalid `perch wt new` invocation: '--no-switch' is not a branch; no branch begins with '-'",
            ),
        ] {
            assert_eq!(command_line(words).unwrap_err().to_string(), message);
        }
    }

    #[test]
    fn a_hook_env_key_must_carry_the_perch_prefix() {
        for words in [
            ["--hook-env", "PATH=/x", "wt"].as_slice(),
            ["wt", "new", "--hook-env", "PATH=/x", "topic"].as_slice(),
        ] {
            assert_eq!(
                command_line(words).unwrap_err().to_string(),
                "`--hook-env` key 'PATH' is not allowed; keys must start with PERCH_"
            );
        }
    }

    #[test]
    fn a_malformed_or_missing_option_value_is_a_usage_error() {
        for (words, message) in [
            (
                ["--hook-env", "NOEQUALS"].as_slice(),
                "invalid `--hook-env` pair 'NOEQUALS'; expected KEY=VALUE with a non-empty KEY",
            ),
            (
                ["--hook-env", "=value"].as_slice(),
                "invalid `--hook-env` pair '=value'; expected KEY=VALUE with a non-empty KEY",
            ),
            (
                ["-C", "repo", "--hook-env"].as_slice(),
                "`--hook-env` needs a value",
            ),
            (["-C"].as_slice(), "`-C` needs a value"),
        ] {
            assert_eq!(command_line(words).unwrap_err().to_string(), message);
        }
    }

    #[test]
    fn parses_each_navigation_intent_and_its_escape() {
        assert_eq!(
            parse(&args(&["topic"])),
            Ok(navigate(Verb::Go, Some("topic")))
        );
        assert_eq!(
            parse(&args(&["br", "topic"])),
            Ok(navigate(Verb::Here, Some("topic")))
        );
        assert_eq!(
            parse(&args(&["wt", "--", "rm"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: None,
                target: Some("rm".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
        );
    }

    #[test]
    fn worktree_navigation_accepts_a_directory_name_before_the_branch() {
        assert_eq!(
            parse(&args(&[
                "wt",
                "--no-switch",
                "545",
                "renovate/realm-swiftlint-0.x",
            ])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("545".into())),
                target: Some("renovate/realm-swiftlint-0.x".into()),
                shell_handoff: ShellHandoff::Suppress,
            }))
        );
    }

    #[test]
    fn worktree_navigation_escape_accepts_a_colliding_directory_name() {
        assert_eq!(
            parse(&args(&["wt", "--", "rm", "topic"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("rm".into())),
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
        );
    }

    #[test]
    fn worktree_navigation_escape_accepts_an_option_looking_directory_name() {
        assert_eq!(
            parse(&args(&["wt", "--", "--noswitch", "topic"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("--noswitch".into())),
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
        );
    }

    #[test]
    fn worktree_navigation_escape_ignores_a_trailing_no_switch() {
        assert_eq!(
            parse(&args(&["wt", "--", "topic", "--no-switch"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: None,
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
        );
    }

    #[test]
    fn worktree_navigation_escape_keeps_other_option_looking_words_as_branches() {
        assert_eq!(
            parse(&args(&["wt", "--", "short", "--typo"])),
            Ok(Invocation::Navigate(Navigation::Worktree {
                worktree_name: Some(WorktreeDirectoryName("short".into())),
                target: Some("--typo".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
        );
    }

    #[test]
    fn worktree_navigation_rejects_an_unknown_option_before_the_name() {
        let error = parse(&args(&["wt", "--noswitch", "topic"])).unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid `perch wt` invocation: unknown option '--noswitch'"
        );
    }

    #[test]
    fn worktree_navigation_rejects_an_extra_argument() {
        let error = parse(&args(&["wt", "one", "topic", "extra"])).unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid `perch wt` invocation: unexpected extra argument 'extra'"
        );
    }

    #[test]
    fn worktree_navigation_rejects_names_that_are_not_one_directory() {
        for name in ["", ".", "..", "nested/name", r"nested\name"] {
            let error = parse(&args(&["wt", name, "topic"])).unwrap_err();

            assert_eq!(
                error.to_string(),
                format!(
                    "invalid `perch wt` invocation: invalid worktree directory name '{name}'; expected one directory name"
                )
            );
        }
    }

    #[test]
    fn worktree_navigation_completes_reachable_branches_in_the_second_position() {
        let Invocation::Complete(completion) =
            parse(&args(&["wt", "short-name", "--complete"])).unwrap()
        else {
            panic!("expected completion");
        };

        assert_eq!(completion.source(), CompletionSource::ReachableBranches);
        assert_eq!(completion.render(["ls", "rm", "topic"]), "ls\nrm\ntopic\n");
    }

    #[test]
    fn worktree_removal_accepts_options_around_one_target() {
        assert_eq!(
            parse(&args(&["wt", "rm", "-f", "topic"])),
            Ok(Invocation::RemoveWorktrees(WorktreeRemoval {
                target: Some("topic".into()),
                force: true,
            }))
        );
    }

    #[test]
    fn parses_branch_removal_list_help_and_version_invocations() {
        assert_eq!(
            parse(&args(&["br", "rm", "--upstream", "topic", "--force"])),
            Ok(Invocation::RemoveBranches(BranchRemoval {
                target: Some("topic".into()),
                force: true,
                upstream: true,
            }))
        );
        assert_eq!(parse(&args(&["wt", "ls"])), Ok(Invocation::ListWorktrees));
        assert_eq!(
            parse(&args(&["br", "--help"])),
            Ok(Invocation::Help(HelpPage::Branch))
        );
        assert_eq!(parse(&args(&["-V"])), Ok(Invocation::Version));
    }

    #[test]
    fn worktree_removal_treats_force_aliases_as_one_option() {
        let error = parse(&args(&["wt", "rm", "-f", "--force"]))
            .expect_err("force aliases should count as duplicates");
        assert_eq!(
            error.to_string(),
            "invalid `perch wt rm` invocation: duplicate option '--force'"
        );
    }

    #[test]
    fn worktree_removal_rejects_unknown_options_and_extra_targets() {
        let unknown = parse(&args(&["wt", "rm", "--remote"])).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "invalid `perch wt rm` invocation: unknown option '--remote'"
        );

        let extra = parse(&args(&["wt", "rm", "one", "two"])).unwrap_err();
        assert_eq!(
            extra.to_string(),
            "invalid `perch wt rm` invocation: unexpected extra target 'two'"
        );
    }

    #[test]
    fn worktree_removal_escape_accepts_a_target_beginning_with_dash() {
        assert_eq!(
            parse(&args(&["wt", "rm", "--", "--force"])),
            Ok(Invocation::RemoveWorktrees(WorktreeRemoval {
                target: Some("--force".into()),
                force: false,
            }))
        );
    }

    #[test]
    fn branch_removal_keeps_its_strict_long_option_grammar() {
        let short = parse(&args(&["br", "rm", "topic", "-f"])).unwrap_err();
        assert_eq!(
            short.to_string(),
            "invalid `perch br rm` invocation: unknown option '-f'"
        );
    }

    #[test]
    fn parses_shell_integration_invocations() {
        assert_eq!(
            parse(&args(&["shell", "init", "fish"])),
            Ok(Invocation::InitShell(Shell::Fish))
        );
        assert_eq!(
            parse(&args(&["shell", "install"])),
            Ok(Invocation::InstallShell)
        );
    }

    #[test]
    fn shell_init_names_an_unsupported_shell() {
        let error = parse(&args(&["shell", "init", "tcsh"])).unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid `perch shell` invocation: unsupported shell 'tcsh'; expected bash, fish or zsh"
        );
    }

    #[test]
    fn completion_filters_only_words_eaten_at_its_position() {
        let candidates = args(&[
            "br", "shell", "wt", "ls", "new", "rm", "list", "remove", "topic",
        ]);
        let Invocation::Complete(bare) = parse(&args(&["--complete"])).unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(
            bare.render(candidates.iter().map(String::as_str)),
            "ls\nnew\nrm\nlist\nremove\ntopic\n"
        );

        let Invocation::Complete(worktree) = parse(&args(&["wt", "--complete"])).unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(
            worktree.render(candidates.iter().map(String::as_str)),
            "br\nshell\nwt\ntopic\n"
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Invocation {
    Complete(Completion),
    Help(HelpPage),
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
        worktree_name: Option<String>,
        target: Option<String>,
        shell_handoff: ShellHandoff,
    },
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
pub(crate) enum HelpPage {
    Branch,
    Main,
    Worktree,
}

impl HelpPage {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Branch => BRANCH_HELP,
            Self::Main => MAIN_HELP,
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
    Escaped,
    Removal,
    Worktree,
}

impl Position {
    fn eats(self, word: &str) -> bool {
        match self {
            Self::Bare => parse_verb(word).is_some(),
            Self::Branch => parse_branch_subverb(word).is_some(),
            Self::Escaped | Self::Removal => false,
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
    #[error("invalid `perch br rm` invocation: {0}")]
    BranchRemoval(String),

    #[error("`--no-switch` does not apply to `perch wt {subverb}`")]
    NoSwitchWithSubverb { subverb: String },

    #[error(
        "`perch wt {word}` is gone; use `perch wt {keep}`, or `perch wt -- {word}` for a branch by that name"
    )]
    Retired {
        word: &'static str,
        keep: &'static str,
    },

    #[error("invalid `perch wt rm` invocation: {0}")]
    WorktreeRemoval(String),

    #[error("invalid `perch wt` invocation: {0}")]
    WorktreeNavigation(String),
}

impl GrammarError {
    fn branch_removal(message: String) -> Self {
        Self(GrammarErrorKind::BranchRemoval(message))
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
    Remove,
    Rm,
}

pub(crate) fn parse(args: &[String]) -> Result<Invocation, GrammarError> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => Ok(Invocation::Help(HelpPage::Main)),
        Some("--version" | "-V") => Ok(Invocation::Version),
        Some("--complete") => Ok(branch_completion(Position::Bare)),
        Some("--") => Ok(parse_escaped(args.get(1), Verb::Go)),
        Some(word) => match parse_verb(word) {
            Some(Verb::Here) => parse_branch(&args[1..]),
            Some(Verb::Worktree) => parse_worktree(&args[1..]),
            Some(Verb::Go) => unreachable!("go is the absence of a command word"),
            None => Ok(navigate(Verb::Go, Some(word))),
        },
        None => Ok(navigate(Verb::Go, None)),
    }
}

pub(crate) fn needs_top_level_escape(word: &str) -> bool {
    parse_verb(word).is_some()
}

fn parse_branch(args: &[String]) -> Result<Invocation, GrammarError> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => Ok(Invocation::Help(HelpPage::Branch)),
        Some("--complete") => Ok(branch_completion(Position::Branch)),
        Some("--") => Ok(parse_escaped(args.get(1), Verb::Here)),
        Some(word) if parse_branch_subverb(word).is_some() => parse_branch_removal(&args[1..]),
        Some(word) => Ok(navigate(Verb::Here, Some(word))),
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

fn parse_worktree(args: &[String]) -> Result<Invocation, GrammarError> {
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
        Some("--") => parse_worktree_navigation(&remaining[1..], shell_handoff, true),
        Some(word) => match parse_worktree_subverb(word) {
            Some(_) if shell_handoff == ShellHandoff::Suppress => {
                Err(GrammarError::no_switch_with_subverb(word.to_string()))
            }
            Some(WorktreeSubverb::Ls) => Ok(Invocation::ListWorktrees),
            Some(WorktreeSubverb::List) => Err(GrammarError::retired("list", "ls")),
            Some(WorktreeSubverb::Remove) => Err(GrammarError::retired("remove", "rm")),
            Some(WorktreeSubverb::Rm) => parse_worktree_removal(&remaining[1..]),
            None => parse_worktree_navigation(&remaining, shell_handoff, false),
        },
        None => Ok(worktree_navigation(None, None, shell_handoff)),
    }
}

fn parse_worktree_navigation(
    args: &[&String],
    shell_handoff: ShellHandoff,
    escaped: bool,
) -> Result<Invocation, GrammarError> {
    match args {
        [] => Ok(worktree_navigation(None, None, shell_handoff)),
        [target] if target.as_str() == "--complete" => Ok(branch_completion(if escaped {
            Position::Escaped
        } else {
            Position::Worktree
        })),
        [target] => Ok(worktree_navigation(None, Some(target), shell_handoff)),
        [worktree_name, target] if target.as_str() == "--complete" => {
            validate_worktree_name(worktree_name)?;
            Ok(branch_completion(Position::Escaped))
        }
        [worktree_name, target] => {
            validate_worktree_name(worktree_name)?;
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

fn validate_worktree_name(name: &str) -> Result<(), GrammarError> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        return Err(GrammarError::worktree_navigation(format!(
            "invalid worktree name '{name}'; expected one directory name"
        )));
    }
    Ok(())
}

fn parse_worktree_removal(args: &[&String]) -> Result<Invocation, GrammarError> {
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

fn parse_escaped(target: Option<&String>, verb: Verb) -> Invocation {
    if target.is_some_and(|word| word == "--complete") {
        branch_completion(Position::Escaped)
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
    worktree_name: Option<&str>,
    target: Option<&str>,
    shell_handoff: ShellHandoff,
) -> Invocation {
    Invocation::Navigate(Navigation::Worktree {
        worktree_name: worktree_name.map(str::to_string),
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

fn parse_verb(word: &str) -> Option<Verb> {
    match word {
        "br" => Some(Verb::Here),
        "wt" => Some(Verb::Worktree),
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
        "remove" => Some(WorktreeSubverb::Remove),
        "rm" => Some(WorktreeSubverb::Rm),
        _ => None,
    }
}

const MAIN_HELP: &str = concat!(
    "Usage: perch [<branch>]       Go to the branch, wherever it lives\n",
    "       perch br [<branch>]    Check the branch out here\n",
    "       perch wt [<branch>]    Give the branch its own worktree\n",
    "       perch wt <name> <branch>\n",
    "                                  Use a custom worktree directory name\n",
    "\n",
    "       perch .                Refresh the current branch from its remote\n",
    "       perch -- <branch>      Go to a branch named br/wt\n",
    "       perch br rm [<branch>] Remove local branches\n",
    "       perch wt ls            List worktrees\n",
    "       perch wt rm [<branch>|.]\n",
    "\n",
    "With the shell integration sourced, `br` and `wt` stand in for `perch br`\n",
    "and `perch wt`. Set PERCH_NO_SHORTCUTS to leave both names alone.\n",
);

const BRANCH_HELP: &str = concat!(
    "Usage: perch br [<branch>]    Check the branch out here\n",
    "       perch br rm [<branch>] [--upstream] [--force]\n",
    "                                  Remove local branches\n",
    "       perch br -- <branch>   Check out a branch named rm\n",
    "\n",
    "Options:\n",
    "      --upstream  Also offer the branch's same-named upstream for removal\n",
    "      --force     Skip destructive confirmations\n",
);

const WORKTREE_HELP: &str = concat!(
    "Usage: perch wt [<branch>] [--no-switch]\n",
    "                                  Give the branch its own worktree\n",
    "       perch wt <worktree-name> <branch> [--no-switch]\n",
    "                                  Use a custom worktree directory name\n",
    "       perch wt ls            List worktrees\n",
    "       perch wt rm [<branch>] Remove a worktree (deletes branch if merged)\n",
    "       perch wt rm .          Remove the worktree you're in\n",
    "       perch wt -- <branch>   Worktree a branch named ls/rm/list/remove\n",
    "       perch wt -- <worktree-name> <branch>\n",
    "                                  Escape a colliding worktree name\n",
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
                worktree_name: Some("545".into()),
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
                worktree_name: Some("rm".into()),
                target: Some("topic".into()),
                shell_handoff: ShellHandoff::Emit,
            }))
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
                    "invalid `perch wt` invocation: invalid worktree name '{name}'; expected one directory name"
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
    fn completion_filters_only_words_eaten_at_its_position() {
        let candidates = args(&["br", "wt", "ls", "rm", "list", "remove", "topic"]);
        let Invocation::Complete(bare) = parse(&args(&["--complete"])).unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(
            bare.render(candidates.iter().map(String::as_str)),
            "ls\nrm\nlist\nremove\ntopic\n"
        );

        let Invocation::Complete(worktree) = parse(&args(&["wt", "--complete"])).unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(
            worktree.render(candidates.iter().map(String::as_str)),
            "br\nwt\ntopic\n"
        );
    }
}

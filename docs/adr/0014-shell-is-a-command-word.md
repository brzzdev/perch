# `shell` is a command word

The shell integration used to be two files a user copied to `~/.config/perch/` and sourced, and nothing kept them in step with the binary they wrap. Building the scripts into the binary, behind `perch shell init <shell>`, and adding one rc line with `perch shell install`, makes the wrapper and the binary one artifact. That needs a top-level word for them, and every top-level word is one more branch name `perch <name>` stops reaching.

## Considered options

- **A flag**, such as `perch --shell-init zsh`. It shadows no branch, since a ref can't start with `-`, but setup would read as an option on navigation. `--version` and `--help` are flags because they're about the binary. Setting up the shell is a command with its own subcommands.
- **Leaving top-level words to the *Verb*s.** [ADR 0007](./0007-three-verbs-one-per-intent.md) declined to guard top-level `worktree` rather than put a branch by that name out of reach. Applied here, that rule would force setup into a separate binary or a flag.

## Consequences

- **A branch named `shell` needs `perch -- shell`**, exactly as one named `br` or `wt` always has. Errors that tell you to run `perch <branch>` already add the `--` for every command word, and bare-position completion leaves the word out for the same reason.
- **Command words and *Verb*s are different sets.** The *Verb* is still one of three navigation intents. `shell` goes nowhere, so it is a command word and not a fourth verb, and `br shell` and `wt shell` still reach a branch called `shell`.
- **The bar for the next one stays high.** Each command word costs a branch name at the top level. A new word has to name something that isn't navigation and couldn't sensibly be a flag.

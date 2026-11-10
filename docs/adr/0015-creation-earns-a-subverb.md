# Creation earns a subverb

Agents create worktrees with `perch -C <repo> --hook-env PERCH_REX_SESSION=<label> wt --no-switch -- <name> <branch>`, and a harness that pre-approves commands by prefix can't approve that safely. The repo comes before the verb, so a rule needs a leading wildcard, and every prefix that covers `wt --no-switch` also covers `wt rm`. `wt new` is a creation-only spelling of `wt --no-switch` that takes its own `-C` and `--hook-env` after the subverb, so the one rule `perch wt new *` approves creating a worktree and nothing else.

## Considered options

- **A top-level command word**, such as `perch new`. Per [ADR 0014](./0014-shell-is-a-command-word.md), every command word costs a branch name at the top level, and the bar is something that isn't navigation. Creating a worktree is navigation, and per [ADR 0007](./0007-three-verbs-one-per-intent.md) its intent already has a *Verb*, `wt`. A second top-level word for the same intent would make the user pick a mode again.
- **Global options after any command word.** `perch wt -C <repo> --no-switch …` would put the repo after the verb, but any prefix still covering `wt` would cover `wt rm` too. The rule needs a word that no destructive form shares.
- **Dropping `wt --no-switch`.** Scripts already use it, and nothing about it is wrong for a human, so it stays.

## Consequences

- **`wt new` is strict where `wt` is forgiving.** It reads `-C` and `--hook-env` until `--` or its first other word, then one target or a *Worktree directory name* and a target, and nothing more. An unknown option, `--no-switch` included, a missing target, a third word, or a target beginning with `-` is a usage error. After `new` no word is a *Subverb*, so `wt new rm topic` makes a directory named `rm`, and a branch named `new` takes `wt -- new`.
- **It never hands off.** It runs the same creation as `wt --no-switch`, with the same fetching, hooks, stale-branch cleanup, and stderr lines, so there is still one creation path.
- **`--hook-env` keys must start with `PERCH_`.** An approved command can then set nothing a hook's shell reads, such as `PATH` or `BASH_ENV`. This holds before the command word too.
- **A worktree path stays beneath the worktrees root, whatever the route.** A directory derived from a branch, as well as a *Worktree directory name*, is checked before perch clears a path, creates a parent, or runs `git worktree add`. An absolute path, `..`, `.`, or an empty component is refused, so an approved command can't remove or create a directory elsewhere.
- **A repo's own config still runs.** `perch.hook.created` and `core.fsmonitor` in the target repo run under the approved rule. Whether hooks should come from global config only is a separate decision.

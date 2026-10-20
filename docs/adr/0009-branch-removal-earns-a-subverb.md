# Branch removal earns a subverb

> The consequence that *Kept* branches are drawn disabled in the picker is superseded by [ADR 0011](./0011-keeping-is-about-the-sweep.md). Held branches are still disabled, and every other consequence stands.
>
> The consequence that the *Equivalent* proof is exclusive to stale cleanup is superseded by [ADR 0012](./0012-equivalence-answers-every-removal.md).

ADR 0007 kept `rm` under `wt` alone because Git already deletes branches, but the resulting command is harder to discover than the operation: `wt rm` strongly suggests a matching `br rm`, and Perch already owns the picker, risk markers, and removal rules that make it more than an alias for `git branch -d`. `br rm` therefore removes local branches, while `wt rm` continues to remove worktrees and their branches. This supersedes only ADR 0007's consequence that `br` gets no `rm`; its three navigation intents remain unchanged.

## Consequences

- **The two removal subverbs share a shape and keep separate boundaries.** Both accept a named target or open a multi-select picker, and both use `--force` only where a warning would otherwise be required. `br rm` refuses a branch held by any worktree; it points to `wt rm` for a linked worktree and asks for another checkout when the main worktree holds the branch. It never removes a worktree or fires a worktree hook.
- **The broad picker shows every local branch.** Held branches and *Kept* branches stay visible but disabled. A precisely named *Kept* branch remains removable when unheld, because `perch.keep` protects broad cleanup rather than forbidding an explicit act. Local merge risk is judged from the main worktree and mirrors `git branch -d`; the stronger *Equivalent* proof remains exclusive to stale cleanup.
- **`rm` is the only spelling `br` reserves.** `br -- rm` reaches a branch literally named `rm`, `br remove` still reaches a branch named `remove`, and `br rm .` has no special meaning. Destructive grammar is strict: unknown flags and extra targets are errors.
- **An upstream is a second, separately licensed removal.** Interactive use offers a same-named, explicitly configured upstream in a separate default-off choice; `--upstream` asks for that choice directly, and non-interactive use also requires `--force`. An untracked branch, a mismatched upstream, and a remote default branch are never offered. Local merged-ness cannot license upstream deletion because it assumes the upstream survives, so every upstream deletion is warned independently and never borrows the local `↑` marker.
- **Local deletion comes first, remote deletion is leased.** Perch captures the upstream tip before removing branch config, deletes the local branch, then deletes the selected upstream only if the local name remains absent and the upstream still has the tip the user saw. A failure leaves the upstream alone or reports the local-only result, processing the rest of a batch before returning a failing status.

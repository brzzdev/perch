# perch

A fast, interactive Git branch and worktree switcher. Pick a branch, fetch & fast-forward it, and clean up merged branches — all in one step.

## Features

- **Interactive branch picker** — fuzzy-select from every branch, with the worktree-backed ones showing their path (or pass a name directly)
- **Auto-stash** — dirty working tree? Changes are stashed before switching and restored after
- **Fast-forward pull** — fetches from origin and fast-forward merges, warns if the branch has diverged
- **Merged branch cleanup** — prompts to delete local branches that have been merged into the current branch, including ones held by a worktree (the worktree goes too)
- **Worktree support** — create, switch into, list, and remove worktrees with `perch wt`; `perch br` is the in-place counterpart

## Install

### Download a binary

Pick your platform and run:

```sh
# macOS (Apple silicon)
TARGET=aarch64-apple-darwin
# macOS (Intel)
TARGET=x86_64-apple-darwin
# Linux (arm64)
TARGET=aarch64-unknown-linux-musl
# Linux (x86_64)
TARGET=x86_64-unknown-linux-musl

curl -fsSL "https://github.com/brzzdev/perch/releases/latest/download/perch-$TARGET.tar.gz" | tar -xz
cd "perch-$TARGET"

# The binary
mkdir -p ~/.local/bin && mv perch ~/.local/bin/

# Shell integration — required for worktree `cd` hand-off (see below)
mkdir -p ~/.config/perch && cp shell/* ~/.config/perch/

# Completions, for zsh
mkdir -p ~/.zsh/completions && cp completions/_perch ~/.zsh/completions/
```

Make sure `~/.local/bin` is on your `PATH`, then add the shell integration line to your rc — see [Shell integration](#shell-integration-required-for-cd) for that and [Shell Completions](#shell-completions) for bash and fish.

The Linux builds are statically linked against musl, so they run on any distribution regardless of its glibc version.

The macOS binaries aren't signed or notarized. Downloading with `curl` as above is fine — quarantine is applied by the downloader, and `curl` doesn't set it. If you download through a browser instead, macOS will refuse to run the binary until you clear the flag:

```sh
xattr -d com.apple.quarantine ~/.local/bin/perch
```

### Build from source

Requires [Rust](https://rustup.rs) and [just](https://github.com/casey/just).

```sh
git clone https://github.com/brzzdev/perch.git
cd perch
just install
```

This builds a release binary and copies it to `~/.local/bin/perch`. Make sure `~/.local/bin` is on your `PATH`.

## Usage

Three verbs, one per intent:

| Command | Meaning |
| --- | --- |
| `perch <branch>` | Take me to it, wherever it lives. Creates nothing. |
| `perch br <branch>` | Check it out **in place**, in this worktree. |
| `perch wt <branch>` | Ensure it has its **own** worktree. |
| `perch wt <name> <branch>` | Give the branch a worktree with a shorter directory name. |

```sh
# Interactive — pick a branch from a list
perch

# Direct — go to a specific branch
perch main

# Refresh the current branch from its remote
perch .
```

Bare `perch <branch>` never has to ask which you meant, because git will not let the same branch be checked out twice: if a worktree already holds it, going there is the only legal move; if none does, checking it out here is.

With no branch named, all three verbs open the **same list** — every branch, with the worktree-backed ones showing their path, so "which of my branches has a worktree?" is answered by looking. The verb changes what Enter does, not what's on offer, and the prompt says which you're in:

```
? Switch to (type to filter):
Local
  > * main
      feature  ~/dev/worktrees/repo/feature
      spike
```

`perch br` draws the same rows, but greys out any branch another worktree holds — it promises a checkout *here*, and git won't allow one — saying inline where the branch went and what reaches it:

```
      feature  in ~/dev/worktrees/repo/feature — use `perch`
```

`perch .` fetches and brings the branch you're on up to date with its remote. A clean branch integrates with no prompt — fast-forwarding, or (when it has diverged, e.g. after rebasing through a web UI) rebasing your local commits onto the remote, which drops any already upstream and replays genuine new work. Only when the working tree is dirty does it stop to ask: **keep** the uncommitted work (stash, rebase, restore) or **discard** it (hard reset to the remote).

## Removing branches

```sh
perch br rm                    # multi-select local branches
perch br rm feature            # remove one local branch
perch br rm feature --upstream # offer origin/feature too, when it is the configured upstream
perch br rm feature --upstream --force
```

The picker lists every local branch. The branch you're on and branches held by another worktree stay visible but cannot be selected. A branch held by a linked worktree points to `wt rm`, which owns removing the worktree along with its branch; the main worktree instead asks you to check out another branch first.

Local rows use the same `↑N` marker and main-worktree merge judgement as `wt rm`. A named unmerged branch asks before using `git branch -D`; a non-interactive run needs `--force`. `br rm` does not apply the stronger squash/rebase equivalence proof used by stale cleanup.

An upstream is offered only when the local branch explicitly tracks a same-named branch, such as `feature` tracking `origin/feature`. Perch never guesses from `origin`, never follows `feature` to a differently named upstream such as `origin/main`, and never offers a remote's default branch. Upstream deletion has its own default-off choice because removing both refs may discard the last names for commits that local deletion alone would preserve. Perch deletes the local branch first, then deletes the upstream only if its server tip has not changed since the choice was shown.

`--upstream` preselects eligible upstreams but keeps the confirmation. `--upstream --force` skips it; both are required outside a terminal. If you really have a branch named `rm`, `perch br -- rm` checks it out and `perch br rm rm` deletes it.

## Worktrees

```sh
# Interactive picker: every branch, worktree-backed ones showing their path.
# Selecting one without a worktree creates it; "Create new: <typed>" when the filter matches nothing.
perch wt

# DWIM — switch into the worktree for `feature`, or create one if it doesn't exist.
# If `feature` doesn't exist as a branch, a new one is created from the remote's default branch.
perch wt feature

# Use a directory name that differs from the branch
perch wt 545 renovate/realm-swiftlint-0.x

# Create or find the worktree, but leave this shell where it is
perch wt feature --no-switch
perch wt 545 renovate/realm-swiftlint-0.x --no-switch

# List all worktrees
perch wt ls

# Remove one or more worktrees, deleting the branch along with them
perch wt rm           # multi-select picker
perch wt rm feature   # specific
perch wt rm .         # the worktree you're standing in
perch wt rm . --force # …without being asked about uncommitted work
```

Worktrees land at `../worktrees/<repo>/<branch>` relative to the main checkout. Branch names with slashes (`feature/foo`) preserve their structure as subdirectories. Pass a separate worktree directory name to use one directory below the repository's worktree root instead, such as `../worktrees/<repo>/545`. The name cannot be `.`, `..`, or contain a path separator. If the branch already has a registered worktree, Perch uses its actual path and ignores the requested name.

If the worktree directory name collides with `ls`, `rm`, `list`, or `remove`, put `--` before it: `perch wt -- rm feature`.

`--no-switch` suppresses the directory handoff to the shell wrapper. Creation, fetching, hooks, and stale-branch cleanup still run as usual.

`wt rm .` removes the worktree you're in and `cd`s you back to the main checkout. If your cwd is the main checkout there's nothing for `.` to name, and it says so.

Removing a worktree destroys two things — a directory and a branch — so anything irreversible is shown before it happens. In the picker, rows carry markers: `●` for uncommitted changes, `↑3` for commits that aren't merged anywhere (a bare `↑` when the branch has no upstream to count against). A marked row is fair warning, so ticking it removes the worktree and deletes the branch outright. An unmarked row has nothing to lose, so it keeps git's own guards — no `--force` on the worktree, a plain `git branch -d` on the branch. If such a worktree turns out to be dirty after all (you changed it while the picker was open), git refuses and says so rather than discarding the work.

A named target like `wt rm .` has no row to carry a marker, so the same information arrives as a confirmation instead:

```
! ~/dev/worktrees/repo/spike has uncommitted changes
! spike has unmerged commits and no upstream
? Remove the worktree and delete spike anyway? [y/N] / esc
```

Nothing at risk means no prompt at all. `--force` (`-f`) skips it. In a pipe or CI run there's no way to show the warning or ask, so a risky removal refuses and exits non-zero unless you pass `--force`.

After a removable worktree passes those guards, Perch moves its directory to a hidden sibling, removes the Git registration, and starts reclaiming the files in the background. The command returns once the original path and registration are gone, so disk space may come back shortly after `wt rm` exits. If background reclamation is interrupted, the next `wt` command retries the exact recorded trash path silently.

Plain `perch <branch>` also knows about worktrees: if the picked branch is already checked out in another worktree, it hands off to that worktree rather than failing with git's "already checked out" error. `perch br <branch>` is the one that won't, since it promises a checkout here:

```
error: feature is checked out at ~/dev/worktrees/repo/feature; run `perch feature` to go there
```

### Shell integration (required for `cd`)

A child process can't change its parent shell's directory. Worktree commands print their target path on stdout; a small shell function reads that and runs `cd` for you.

```sh
just install-shell-integration
# Then add the printed line to your shell rc (e.g. ~/.zshrc):
#   source ~/.config/perch/perch.sh
```

Installing from a release tarball? The same files ship in its `shell/` directory — copy them to `~/.config/perch/` and source the one for your shell (`perch.sh` for zsh and bash, `perch.fish` for fish).

On zsh, put the `source` line **after** `compinit`. The wrapper registers the `br` and `wt` completions as it defines those functions, and `compinit` has to have run for it to do so — source it earlier and the two shortcuts work but don't complete.

Without the wrapper, `perch wt foo` still creates / finds the worktree and prints its path — you'd just `cd` there manually.

### `br` and `wt` shortcuts

Sourcing the wrapper also defines `br` and `wt`, so the two verbs are reachable without typing `perch` first:

```sh
wt feature      # perch wt feature
wt rm .         # perch wt rm .
br main         # perch br main
```

They call the `perch` function rather than the binary, so the `cd` hand-off comes with them.

[broot](https://dystroy.org/broot) also installs a `br` function, and whichever is sourced last wins. Set `PERCH_NO_SHORTCUTS` to any non-empty value *before* the `source` line to leave both names alone:

```sh
# zsh and bash
PERCH_NO_SHORTCUTS=1
source ~/.config/perch/perch.sh
```

```fish
# fish
set -gx PERCH_NO_SHORTCUTS 1
source ~/.config/perch/perch.fish
```

It covers the completions too, so nothing offers branch names for someone else's `br` — perch never claims either name unless it also defines it. `just install-completions` won't take a name it doesn't already own either: where a `br` or `wt` completion file is already there, it says so and leaves it.

## Shell Completions

Tab completions are available for zsh, bash, and fish, and cover `br` and `wt` as well as `perch`. They offer every branch the command would accept in that position — including branches that exist only on the remote, which the picker lists and a bare `perch <name>` will check out.

```sh
just install-completions
```

This installs the appropriate completion script for your current shell. From a release tarball, the same scripts are in its `completions/` directory:

| Shell | File | Destination |
| ----- | ---- | ----------- |
| zsh | `_perch` | `~/.zsh/completions/_perch` |
| bash | `perch.bash` | `~/.local/share/bash-completion/completions/perch` |
| fish | `perch.fish` | `~/.config/fish/completions/perch.fish` |

bash and fish autoload completions by command name, so `br` and `wt` need the installed file to exist under their own names too — `just install-completions` does this for you:

```sh
# bash
ln -s perch ~/.local/share/bash-completion/completions/br
ln -s perch ~/.local/share/bash-completion/completions/wt

# fish
ln -s perch.fish ~/.config/fish/completions/br.fish
ln -s perch.fish ~/.config/fish/completions/wt.fish
```

Plain `ln -s`, not `ln -sf`: if one of those names is already taken, the link should fail rather than replace whatever owns it. `install-completions` behaves the same way and tells you which name it left alone.

zsh needs no links — it claims completions by name at `compinit` time, and `_perch` deliberately claims only `perch`.

Completing `br` and `wt` needs the [shell integration](#br-and-wt-shortcuts) as well, in every shell. Perch completes a shortcut only while that name still resolves to the function its wrapper defined, and it stands down per name — take `br` for something else and `wt` is unaffected. Install the completions alone and only `perch` completes, which is the honest answer, since without the wrapper the shortcuts don't exist.

zsh and bash re-read that on every completion. fish decides as its completion file loads, which is the first time you complete one of the three names — so a `br` claimed *after* that, later in the same session, is the one case fish keeps answering for. `complete -e -c br` clears it. fish's `--wraps` is what lets one set of rules serve all three names, and it offers no hook to re-check per keystroke.

For zsh, make sure `~/.zsh/completions` is in your `fpath` before `compinit`:

```sh
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

## Cleaning up stale branches

After a switch, `perch` offers to delete branches that have outlived their purpose — those whose work has landed on your default branch, and those whose upstream has been deleted. Nothing is judged against the worktree you happen to be standing in: the default branch is the yardstick. A branch you just created and haven't committed to isn't offered, unless you cut it from a default branch that was itself ahead of or behind its remote, in which case it borrows that branch's position and can be — see [ADR 0002](docs/adr/0002-staleness-is-anchored-to-the-default-branch.md) for why no amount of ref-reading tells the two apart. A branch checked out in another worktree appears here too, annotated with the worktree that holds it; deleting it removes that worktree as well:

```
? Delete stale branches (space to toggle, →/← all/none)
  > [ ] chore/deps       (+ worktree ●)
    [ ] fix/typo
    [ ] spike/abandoned  ↑
```

The markers mean the same thing as in `wt rm`: `●` for a worktree with uncommitted changes, `↑` for commits that aren't merged anywhere — the case where a branch's remote was deleted while it still held unpushed work. `(+ worktree, missing)` marks a leftover registration whose directory is already gone. Every deletion reports itself, naming the path of any worktree that was removed.

A branch that landed by squash or rebase merge draws no `↑`, and is deleted without being asked about. Git considers it unmerged — its commits are nowhere in your default branch under those hashes — but its work is there under other ones, so the warning would name commits nothing can lose. `perch` proves that for itself, locally: either the anchor already carries the branch's patch (git's own test, the one `git rebase` uses to drop redundant commits), or the files the branch touched now read identically there. It only ever *removes* a warning — a branch it can't prove keeps its marker, and one whose proof it can't establish at all is treated as holding unique work. See [ADR 0005](docs/adr/0005-proof-of-equivalence-is-a-license.md).

## Configuration

Hold branches back from the "delete merged branches" prompt by adding them to your Git config:

```sh
git config --add perch.keep develop
git config --add perch.keep staging
```

### Worktree hooks

Run a shell command when a worktree is created or removed — to open it in an editor, register it with a session manager, or forget it again when it goes:

```sh
git config perch.hook.created 'my-editor open "$PERCH_WORKTREE"'
git config perch.hook.removed 'my-editor forget "$PERCH_WORKTREE"'
```

Each command runs via `sh -c` from the main checkout, with:

| Variable | Value |
| -------- | ----- |
| `PERCH_BRANCH` | The worktree's branch, empty for a detached one |
| `PERCH_EVENT` | `created` or `removed` |
| `PERCH_MAIN` | Absolute path of the main checkout |
| `PERCH_WORKTREE` | Absolute path of the worktree the event is about |

`created` fires the moment the worktree exists, before the stale-branch prompt. `removed` fires once per worktree, only after one is actually gone — so the path it names no longer exists on disk. Both cover every route: `removed` fires for a worktree taken along by the stale-branch cleanup just as it does for `wt rm`, since a hook mirrors what happened to the repo rather than which command you typed.

A hook is told what happened; it is never asked. It cannot refuse a removal or license a forcing, and a non-zero exit is warned about and otherwise ignored. Its stdout is re-emitted on stderr, since stdout carries the path the shell wrapper `cd`s into; its stderr passes through. Set `PERCH_NO_HOOKS=1` — or any non-empty value — to turn hooks off for a run. See [ADR 0003](docs/adr/0003-hooks-are-told-never-asked.md) for the reasoning.

## License

MIT — see [LICENSE](LICENSE).

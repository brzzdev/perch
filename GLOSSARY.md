# perch

An interactive Git branch and worktree switcher. Its domain is the small set of judgements it makes on the user's behalf: which branches have outlived their purpose, which worktrees can go, and what may be destroyed without asking first.

## Language

### Branches

**Anchor**:
The ref staleness is judged against: the local default branch, or its remote counterpart where there is no local copy. Never the current branch — with worktrees in play, "current" is an accident of which directory you are standing in. Where no anchor resolves, the merged rule stands down and only a deleted upstream marks a branch stale. See [ADR 0002](./docs/adr/0002-staleness-is-anchored-to-the-default-branch.md).
_Avoid_: Mainline, main line, HEAD, trunk

**Stale**:
A branch that has outlived its purpose — its work has landed on the anchor, or its upstream has been deleted. Topology cannot show landing, since a branch cut from the anchor looks exactly like one the anchor absorbed, so it is read from what the branch *tracks*: either it tracks the anchor's counterpart while *ahead* of it, or it tracks nothing and its tip is reachable from, but not equal to, the anchor's tip. Neither applies to a branch published under a name of its own, which waits for its upstream to be deleted. Both are proxies: a branch cut from an anchor that was already ahead or behind borrows that position and is offered though it holds nothing. Staleness is what qualifies a branch for the cleanup prompt; the ref fact that qualified it is the branch's *Ground*. It says nothing about whether deleting it is safe.
_Avoid_: Dead, old, obsolete

**Ground**:
The ref fact that put a *Stale* branch on the cleanup prompt. *Tracks anchor, ahead* means it tracks the anchor's counterpart and is ahead of it. *Untracked, tip in anchor* means it has no upstream and its tip is reachable from, but not equal to, the anchor's tip. *Gone* means its upstream was deleted. The facts never overlap: a deleted upstream is read first and is never tested against the anchor. A ground is not a *Risk*. It says why a branch is offered, never what deleting it would destroy, so the picker writes it as words and never draws it as a *Marker*. See [ADR 0004](./docs/adr/0004-a-ground-is-not-a-marker.md).
_Avoid_: Reason, cause, landed, merged (reserved for topology, which staleness deliberately does not read)

**Unmerged**:
Holding commits that `git branch -d` would refuse to discard. Git's rule is *"fully merged in its upstream branch, or in HEAD if no upstream was set"* — alternatives, not a pair: where an upstream exists it alone decides, so a branch merged into HEAD but ahead of its upstream is still unmerged. A branch can be both stale and unmerged: work merged into the anchor locally, before the anchor was pushed. Being *Equivalent* does not make a branch merged — git would still refuse it — it only means refusing costs nothing.
_Avoid_: Unpushed, ahead, dirty

**Equivalent**:
A branch whose whole diff against the anchor is already in the anchor, under some other commit — squash-merged, rebase-merged, or cherry-picked. It is the one judgement read from *content* rather than from what a branch tracks or where its commits sit, and it is positive evidence: where it cannot be established, the branch is treated as holding unique work. Equivalence only ever subtracts a *Risk* from a branch already on the cleanup prompt, never adds a branch to it. See [ADR 0005](./docs/adr/0005-proof-of-equivalence-is-a-license.md).
_Avoid_: Squashed, duplicate, redundant

**Kept**:
Held back from the *Stale* cleanup prompt, via `perch.keep` config or by being the remote's default branch. Keeping is about the sweep, not the branch: `br rm` and `wt rm` reach a kept branch like any other, and a kept branch is never drawn differently in a picker. See [ADR 0011](./docs/adr/0011-keeping-is-about-the-sweep.md).
_Avoid_: Protected, ignored, excluded

### Worktrees

**Held**:
Of a branch, checked out in some worktree. Git forbids the same branch in two worktrees, so a held stale branch is always held by a worktree other than the one you're in.
_Avoid_: Locked, checked out, in use

**Missing**:
A worktree still registered in `.git/worktrees` whose directory is gone — git calls this *prunable*. It cannot be entered, and it blocks its branch from being checked out or deleted — but only until something prunes the dead registration, which both `checkout` and `worktree add` do for themselves before retrying. So it blocks git and not `perch`, which is why the *Catalogue* treats it as holding nothing.
_Avoid_: Stale (reserved for branches), dead, orphaned

**Dirty**:
Of a worktree, holding uncommitted changes: tracked edits or untracked, non-ignored files.
_Avoid_: Modified, unclean

**Main worktree**:
The original checkout, which git will not let you remove. Every other worktree is *removable*.
_Avoid_: Root, primary, parent

**Trash**:
The renamed remains of a worktree waiting for *Reclamation*. It no longer acts as a worktree, but its files survive until reclamation finishes.
_Avoid_: Cleanup directory, temporary worktree

**Staged**:
The state of *Trash* while its *Removal* is still undecided. Staged trash must remain restorable and cannot be reclaimed.
_Avoid_: Pending (reserved for a Removal awaiting its finish), ready

**Ready**:
The state of *Trash* after Git has confirmed the worktree registration is gone. Ready trash may be reclaimed.
_Avoid_: Staged, removable

### Destruction

**Risk**:
What removing something would irreversibly destroy: a dirty worktree's files, an unmerged branch's commits, or a shared upstream ref. Something with no risk can be removed without asking. An unmerged branch that is *Equivalent* destroys nothing, so it carries no risk and draws no *Marker*; an upstream deletion always carries risk because the local merge judgement assumes that ref survives.
_Avoid_: Danger, safety, hazard

**Marker**:
The rendering of a risk in a picker row: `●` for dirty, `↑N` for unmerged. A marker is a warning, and per [ADR 0001](./docs/adr/0001-warned-means-forceable.md) a shown warning is what licenses forcing. Upstream deletion has no marker: its separate picker or confirmation is the warning. `wt ls` draws from the same vocabulary, so a glyph looks the same wherever it appears — but `↑N` there counts commits the upstream lacks, which is not the same judgement as *Unmerged* and licenses nothing. Sharing the glyphs is not sharing the facts.
_Avoid_: Flag, badge, indicator

**Forcing**:
Destroying something git would otherwise protect, or skipping Perch's upstream-deletion confirmation when `--upstream` also requests it. Only ever licensed by a warning the user has already seen, an explicit `--force`, or, for a local branch alone, proof that it is *Equivalent*.
_Avoid_: Overriding, ignoring

**License**:
What permits forcing: the markers already shown to the user, the separate upstream choice, an explicit `--force`, or proof that a branch is *Equivalent*. A warning or proof covers what it named and nothing else; explicit `--force` covers the whole *Removal*. Anything that became risky after its row was drawn meets git's own guard instead — as does a proof whose ground has shifted, since equivalence is established on a pair of commits and lapses when either the branch or the *Anchor* moves off it. See [ADR 0001](./docs/adr/0001-warned-means-forceable.md), [ADR 0005](./docs/adr/0005-proof-of-equivalence-is-a-license.md), and [ADR 0009](./docs/adr/0009-branch-removal-earns-a-subverb.md).
_Avoid_: Permission, approval, consent

**Removal**:
Destroying a local branch, an upstream branch, a worktree, or a local branch together with the worktree holding it. Every destructive path first *assesses* repository facts, then records the user's *choice*, and only then *finishes* the chosen work. Assessment makes no destructive change; any Git objects it creates are unreachable and have no user-visible effect. Abandoning a pending removal cancels it. A worktree goes before the local branch it holds; an upstream branch goes only after its selected local branch, and only while the upstream still stands at the tip the user was shown.
_Avoid_: Deletion (reserved for branches), cleanup, teardown

**Reclamation**:
Recovering disk space from a removed worktree's *Trash*. It follows *Removal* and may finish after the command that removed the worktree returns.
_Avoid_: Cleanup (reserved for the stale-branch prompt), Removal, deletion

### Targets

**Verb**:
Which of three navigation intents a command carries: bare `perch` goes to the branch wherever it lives, `br` checks it out in the worktree you're in, `wt` gives it one of its own. The verb decides what happens to a *Held* branch and nothing else, since git leaves exactly one move legal in every other case. That holds in the *Catalogue* too: all three verbs draw the same list, and the only row any of them differs over is a held one. See [ADR 0007](./docs/adr/0007-three-verbs-one-per-intent.md) and [ADR 0008](./docs/adr/0008-one-list-whichever-verb-is-picking.md).
_Avoid_: Mode, action

**Catalogue**:
Every branch the repo offers, paired with the worktree *Held*ing it — a *Missing* one excepted, since it can be pruned out of the way and so holds nothing anything here has to route around. It is what the picker is built from and is the same for all three *Verb*s — a verb changes what selecting a row does, never what is listed. Turning it into rows is pure: given the branches and the worktrees, the *Annotation*s and the unselectable rows follow. A catalogue is a snapshot, so what is decided *from* one — where to hand the shell off, above all — reads the worktrees again rather than trusting it. See [ADR 0008](./docs/adr/0008-one-list-whichever-verb-is-picking.md).
_Avoid_: Candidates, options, menu

**Annotation**:
The dim text after a name in a picker row, saying what there is to know about it: the path of the worktree *Held*ing a branch, or why a row is inert. An annotation is not a *Marker* — it warns of no loss and licenses no *Forcing*, exactly as a *Ground* doesn't. Rows share a column for it, so a list reads down as well as across. See [ADR 0004](./docs/adr/0004-a-ground-is-not-a-marker.md) and [ADR 0008](./docs/adr/0008-one-list-whichever-verb-is-picking.md).
_Avoid_: Marker (reserved for risk), label, badge, hint

**Worktree directory name**:
The final directory component reserved for a branch's new worktree. It normally follows the branch spelling, but a user may choose a separate name; it does not rename the branch or override an existing worktree already *Held*ing it.
_Avoid_: Worktree name, branch name, path, folder

**Subverb**:
A word a *Verb* reads before it reads a branch name. `br` reads `rm`; `wt` reads `ls` and `rm`, plus the retired `list` and `remove`, which are refused rather than taken for branches. Collision is positional: `--` is needed after `br` for a colliding branch, after `wt` for either a colliding branch or a *Worktree directory name* that looks like a subverb or option, and at the top level for a branch matching a *Verb*. Everywhere else the bare name reaches the branch, so `perch list`, `perch br list` and `perch br wt` all work as written.
_Avoid_: Subcommand (reserved for the *Verb*), flag, option

**Grammar**:
The rules that decide whether each command word names a *Verb*, *Subverb*, option, *Worktree directory name*, or branch. `--` makes the next word a branch at the top level and after `br`; after `wt` it introduces either one branch or a *Worktree directory name* followed by a branch without reading options or subverbs, except that the legacy `wt -- <branch> --no-switch` stays a single-branch form and does not suppress the *Handoff*; inside `wt rm` it introduces a target. Destructive forms reject duplicate options, unknown options, and extra targets; worktree creation rejects unknown options, invalid *Worktree directory names*, and extra arguments.
_Avoid_: Dispatch, parsing

**`.`**:
The one you're in. `perch .` refreshes the current branch; `perch wt rm .` removes the current worktree. `br rm .` has no special meaning because a branch cannot delete itself out from under its worktree.
_Avoid_: Here, current, self

**Handoff**:
Printing a directory to stdout for the shell wrapper to `cd` into, since a process cannot change its parent's working directory. `wt --no-switch` suppresses the handoff after creating or finding a worktree, so the shell stays where it is.
_Avoid_: Jump, teleport, redirect

### Hooks

**Hook**:
A user command run after a worktree is created or removed. A hook is told what happened; it is never asked. It cannot refuse a *Removal*, grant a *License*, or change anything `perch` does — one that fails is reported and ignored. See [ADR 0003](./docs/adr/0003-hooks-are-told-never-asked.md).
_Avoid_: Callback, plugin, trigger, integration

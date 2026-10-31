# Proof of equivalence is a license

> [ADR 0012](./0012-equivalence-answers-every-removal.md) adds a route ahead of the others: an anchor that already reaches the branch's tip proves it, which proves an untouched branch too. Every other consequence stands.

A squash-merged branch is unmerged by every test git offers: its commits are not ancestors of the
anchor, so `git branch -d` refuses it and the picker draws `↑N` over work that already landed. The
warning is true and useless — the commits it names are irrecoverable only in the sense that their
*hashes* are, and their content sits in the anchor under another name. So git-switch proves the
content case for itself: a branch whose whole diff against the anchor is already present there as
some other commit is *Equivalent*, carries no *Risk*, draws no *Marker*, and is deleted without
being asked about. That makes proof a third source of *License* alongside the markers shown and an
explicit `--force`, which [ADR 0001](./0001-warned-means-forceable.md) did not anticipate.

## Considered options

- **A distinct glyph** — mark equivalent branches with their own symbol rather than nothing. Rejected:
  it would be the only marker in the vocabulary that warns of nothing, and the interruption the
  feature exists to remove would survive in a new costume.
- **Redefining *Unmerged*** — teach it that equivalent branches aren't unmerged. Rejected: *Unmerged*'s
  worth is that it predicts `git branch -d` exactly, and the safe-delete path is built on that
  prediction. A branch git will refuse must keep being called unmerged.
- **Asking the forge** — query `gh`/`glab` for the merged pull request. Rejected: it needs a network,
  credentials, and a forge we recognise, to answer a question local git can answer alone.

## How it is proven

Three routes, and any one proves it, because work lands in more shapes than one test sees (ancestry, from ADR 0012, comes before all three):

- **The patch already landed.** The branch's whole diff over its merge-base is synthesised as one
  commit and handed to `git cherry`, which answers by patch id. This survives the anchor moving on
  over the same files, and it is the only route that answers a squash merge — but it reads the diff
  as a single patch, so a rebase-merge that replayed several commits individually defeats it.
  `cherry` is not the whole answer, because the patch ids it compares are normalised: they ignore
  whitespace, and a branch differing from what landed by whitespace alone would pass. That is the
  right trade for `git rebase`, which drops such a commit but leaves the branch behind to recover
  it from; it is the wrong one for a force-delete. So `cherry` — asked the other way round — is used
  to *name* the commit carrying the patch, and the two are then compared with `patch-id --verbatim`,
  which weighs whitespace but still ignores line numbers, so an anchor that has drifted does not
  cost the proof. `--verbatim` arrived in git 2.39; an older git fails that step, which is a failed
  step like any other and leaves this route proving nothing at all.
- **The content is present.** Every path the branch touched since the merge-base reads
  byte-identically on the anchor — every path as git records them, renames left undetected, since a
  rename reported as its destination alone would hide the deletion of its source. Blind to how the
  work arrived, so a rebase-merge or a scattered cherry-pick answers it; broken by any later edit to
  those files, which is why it cannot stand alone. A branch that touched nothing has no paths to
  compare and is proven by none of the three. *Every* diff a route reads — the exact comparison
  included — is asked for in terms configuration cannot narrow: submodules unignored, no textconv,
  no external driver. A proof read from a diff the repository chose to shrink is a branch deleted
  for work the anchor never took, and an "exact" comparison of what a driver renders is not exact
  at all.
- **Merging would add nothing.** `git merge-tree --write-tree` merges the tip into the anchor without
  touching a worktree. If that merge is clean and its tree is exactly the anchor's, the branch holds
  nothing a merge would keep, so deleting it loses nothing. This answers the shapes the other two
  routes miss: a rebase-merge, or a squash, that the anchor has since edited around in the same files.
  It is tried after them, never instead of them. A branch the anchor took and then partly reverted is
  proven only by the patch route, because merging it again would bring the reverted half back. A merge
  driver can make a merge add nothing by throwing away the branch's side: an `ours`-style driver, or
  `union` keeping a line the branch deleted. So a path the branch touched whose `merge` attribute
  names a driver, or a `merge.default` that names one for every path, proves nothing on this route.
  `-merge` selects the built-in binary driver, which conflicts rather than picking a side, so it is
  allowed. Rename detection is off for the simulation so that every content merge happens at a path
  the branch touched, which is where the attributes were checked. Renormalisation is off too, so a
  branch whose change was only line endings cannot merge away. A conflict, any other failure, or a git
  older than 2.38 (which has no `--write-tree`) is an inconclusive proof. Like the other routes, this
  one asks about the same pair of resolved commits the proof records.

## Consequences

- **Equivalence only ever subtracts.** It removes a warning from a branch already on the cleanup
  prompt; it never puts one there. *Stale* keeps reading what a branch tracks, never its content —
  a branch cut from the anchor and never committed to is trivially equivalent, and would otherwise
  be offered for deletion the moment it was created.
- **The proof is pinned to both commits it was made from.** *License* covers what was established
  and nothing more, and equivalence is established on a pair: where the branch stood, and what the
  anchor held. Both are re-checked before the force-delete. A branch that grew a commit holds work
  nobody proved; an anchor rewound in the meantime — by a *Hook* fired for an earlier row, say — no
  longer holds the content that made the branch safe to discard. Either lapse drops the delete to
  `-d`, to meet git's own guard exactly as an unmarked worktree does. The branch half is checked
  *by* the delete rather than before it — `update-ref -d <ref> <oid>` compares and deletes as one
  operation, so a commit made in the gap between the two cannot be discarded unwarned. The anchor
  half has no such gap to close, since no single git command speaks for two refs; that window stays
  open and is accepted, being a rewind of the anchor within the seconds a prompt is on screen.
- **Deleting by hand means keeping git's guards by hand.** `update-ref` is plumbing: it does not
  refuse a branch a worktree has checked out, and it does not take the branch's config with it, both
  of which `branch -D` does. So the holder is looked for either side of the delete — the ref is put
  back where a worktree won the race, since the branch a worktree checked out must still be there —
  and the config is cleared key by key, because the section-wide removal git offers cannot parse
  every name git allows. What outlived the branch is read back rather than inferred from exit codes,
  and reported rather than swallowed: the branch has gone, and clearing the rest is now the user's
  to do. Past the delete nothing may fail silently either — a restore that cannot be performed is an
  outcome of its own, since a worktree left on a missing ref is the one state a message cannot
  repair, and so is a config that could not be inspected, which is not the same claim as a config
  left behind. Each of them leads with the deletion when it is worded: the branch has gone, and a
  line saying otherwise sends the user looking for a ref that isn't there.
- **An inconclusive proof means unmerged.** Equivalence is positive evidence that defeats a warning;
  where it cannot be established — no anchor, a failing `merge-base` — the warning stands, silently
  and indistinguishably from a branch that genuinely holds unique work.
- **How a branch landed stops being visible.** With no marker, a squash merge and a real merge look
  alike in the picker. The picker answers "may this go?", not "how did it get in?".
- **Answering costs a write.** The proof synthesises the branch's diff as one commit via
  `commit-tree`, which puts a dangling object in the repository — the only mutation git-switch makes
  in order to observe something. It is unreachable and gc reaps it.

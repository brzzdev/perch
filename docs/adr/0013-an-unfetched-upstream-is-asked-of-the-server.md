# An unfetched upstream is asked of the server

A clone whose `remote.<name>.fetch` covers only some branches, as `git clone --single-branch` leaves it, still records an upstream for every branch pushed with `-u`. Git resolves `%(upstream)` through the refspec, so for such a branch it reports none, and `git branch -d` judges it against HEAD instead. HEAD never holds a topic branch's own commits, so `wt rm .` on a branch whose every commit is on the server warned that it had "no upstream" and asked to force. Perch now reads `branch.<name>.remote` and `.merge` itself, asks that remote's server where the upstream stands, and treats a server copy that already holds the branch as proof that deleting it costs nothing.

## Considered options

- **Fetching the ref** — write `refs/remotes/<remote>/<branch>` and let git judge as usual. Rejected: `-d` finds an upstream through the refspec, not by the ref existing, so it would still fall back to HEAD, and perch would be leaving refs the user's config never asked for.
- **A truthful warning only** — name the unfetched upstream and keep asking. Rejected: the warning is true and the question useless, which is the failure [ADR 0012](./0012-equivalence-answers-every-removal.md) removed for squash merges.

## Consequences

- **It is a proof, with a proof's re-check.** The server tip is the anchor and is read again, by `ls-remote`, just before the delete, which is pinned to the branch tip that was proven, exactly as [ADR 0005](./0005-proof-of-equivalence-is-a-license.md) records. A server copy that moved off what was proven drops the delete to `-d`.
- **Removal can reach the network.** [ADR 0005](./0005-proof-of-equivalence-is-a-license.md) rejected asking the forge partly because it needs a network. This asks git's own transport, once per remote, and only for unmerged branches with an upstream git cannot see, which perch's equivalence proof could not already clear. Only a named target asks: a picker or the stale sweep would wait on the network before drawing, and a row's marker already licenses forcing it there. A remote that cannot be reached proves nothing, and the branch keeps its warning.
- ***Unmerged* is unchanged.** It still predicts `-d`, which refuses such a branch, so an unfetched upstream is a third way to be unmerged without a count. Its warning says the upstream is not fetched and the remote did not confirm it holds the commits, so a user can tell it from a branch that was never pushed.

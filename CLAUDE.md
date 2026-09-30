# perch

Interactive Git branch and worktree switcher with merged branch cleanup, written in Rust.

## Releasing

`just release <tag>` opens the version-bump PR. Run it again once that has merged, to tag and
trigger the release build.

## Style

- Rust 2024 edition
- Keep dependencies minimal

## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues, driven by `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Research notes

Findings from investigating a question go in `docs/research/`, one file per question, styled like
the ADRs. Cite files, not line numbers — line numbers rot the moment the code moves.

# prettier-ignore

[private]
default:
  just --list

# Format the source code.
format:
  cargo fmt

# Install the release binary locally.
install: build-release
  mkdir -p ~/.local/bin
  @# `install` writes a fresh file where `cp` would overwrite in place, leaving
  @# macOS a stale code signature that kills the new binary on launch.
  install target/release/perch ~/.local/bin/perch

# Install shell completions for perch, br, and wt.
install-completions:
  #!/usr/bin/env sh
  # One file per shell covers all three names. zsh reads them off the `#compdef`
  # line, but bash and fish autoload by command name, so `br` and `wt` each need
  # the file to exist under their own name — a symlink, so there is one copy.
  #
  # A name we don't already own belongs to someone else — broot ships its own
  # `br` — so leave it be and say so rather than silently taking the name over.
  link_shortcut() {
    if [ -L "$2" ] && [ "$(readlink "$2")" = "$1" ]; then
      return 0
    fi
    if [ -e "$2" ] || [ -L "$2" ]; then
      echo "warning: $2 already exists and isn't ours — leaving it alone;" >&2
      echo "         perch will not complete \`$(basename "$2" .fish)\`." >&2
      return 0
    fi
    ln -s "$1" "$2"
  }
  case "$(basename "$SHELL")" in \
    zsh) \
      mkdir -p ~/.zsh/completions && \
      cp completions/_perch ~/.zsh/completions/_perch && \
      echo "Installed zsh completion to ~/.zsh/completions/_perch" && \
      echo "Ensure ~/.zsh/completions is in your fpath. Add to ~/.zshrc:" && \
      echo '  fpath=(~/.zsh/completions $fpath)' && \
      echo '  autoload -Uz compinit && compinit' ;; \
    bash) \
      mkdir -p ~/.local/share/bash-completion/completions && \
      cp completions/perch.bash ~/.local/share/bash-completion/completions/perch && \
      link_shortcut perch ~/.local/share/bash-completion/completions/br && \
      link_shortcut perch ~/.local/share/bash-completion/completions/wt && \
      echo "Installed bash completion." ;; \
    fish) \
      mkdir -p ~/.config/fish/completions && \
      cp completions/perch.fish ~/.config/fish/completions/perch.fish && \
      link_shortcut perch.fish ~/.config/fish/completions/br.fish && \
      link_shortcut perch.fish ~/.config/fish/completions/wt.fish && \
      echo "Installed fish completion." ;; \
    *) \
      echo "Unsupported shell: $SHELL" && exit 1 ;; \
  esac

# Install the shell wrapper function (required for worktree cd hand-off).
install-shell-integration:
  #!/usr/bin/env sh
  mkdir -p ~/.config/perch
  case "$(basename "$SHELL")" in \
    zsh|bash) \
      cp shell/perch.sh ~/.config/perch/perch.sh && \
      echo "Installed shell integration to ~/.config/perch/perch.sh" && \
      echo "Add to your shell rc:" && \
      echo "  source ~/.config/perch/perch.sh" ;; \
    fish) \
      cp shell/perch.fish ~/.config/perch/perch.fish && \
      echo "Installed shell integration to ~/.config/perch/perch.fish" && \
      echo "Add to ~/.config/fish/conf.d/perch.fish:" && \
      echo "  source ~/.config/perch/perch.fish" ;; \
    *) \
      echo "Unsupported shell: $SHELL" && exit 1 ;; \
  esac

# Build a release binary.
build-release:
  cargo build --release

# Release a version. Run once to open the version-bump PR, again once it has merged to tag.
release tag:
  #!/usr/bin/env sh
  set -e
  version="{{tag}}"
  version="${version#v}"
  original="$(git rev-parse --abbrev-ref HEAD)"

  # A dirty tree would drag unrelated work into the bump commit.
  if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty; commit or stash first" >&2
    exit 1
  fi

  git fetch --quiet origin main --tags

  # The tag is the one irreversible step of a release, so never reuse one.
  if git rev-parse -q --verify "refs/tags/v$version" >/dev/null \
    || [ -n "$(git ls-remote --tags origin "refs/tags/v$version")" ]; then
    echo "error: tag v$version already exists" >&2
    exit 1
  fi

  # Which phase we are in is read from main itself rather than tracked
  # anywhere, so an interrupted release resumes just by re-running.
  main_version="$(git show origin/main:Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
  max_version="$(printf '%s\n%s\n' "$version" "$main_version" | sort -V | tail -1)"
  if [ "$version" != "$max_version" ]; then
    echo "error: $version is not newer than $main_version, already on main" >&2
    exit 1
  fi

  # Phase 1 — main is still on the old version, so open the bump PR. It has to
  # go through a PR: main requires signed commits and a passing `test` check,
  # and admin bypass is scoped to pull requests only.
  if [ "$main_version" != "$version" ]; then
    branch="release/$version"

    # An earlier run may have stopped part-way — branch committed or pushed,
    # but `gh pr create` never landed. Pick up from wherever it stopped rather
    # than dead-ending on "a branch named ... already exists".
    if git rev-parse -q --verify "refs/heads/$branch" >/dev/null \
      || [ -n "$(git ls-remote --heads origin "$branch")" ]; then
      if [ -n "$(gh pr list --head "$branch" --state open --json number --jq '.[].number')" ]; then
        echo "error: the bump PR for $version is already open; merge it, then re-run to tag" >&2
        exit 1
      fi
      echo "resuming: $branch already exists with no open PR"
    else
      git checkout --quiet -b "$branch" origin/main
      perl -i -pe "s/^version = \".*\"/version = \"$version\"/" Cargo.toml
      cargo check --quiet
      git add Cargo.toml Cargo.lock
      git commit --quiet -m "chore: bump version to $version"
      # Back to where you started before anything can fail, so an interrupted
      # run never strands you on the release branch.
      git checkout --quiet "$original"
    fi

    if git rev-parse -q --verify "refs/heads/$branch" >/dev/null; then
      git push --quiet -u origin "$branch"
    fi
    gh pr create --base main --head "$branch" \
      --title "chore: bump version to $version" \
      --body "Version bump for the v$version release."
    echo "Merge the PR, then run: just release $version"
    exit 0
  fi

  # Phase 2 — the bump has landed, so tag it. Tags sit outside the branch
  # ruleset, so this needs no PR.
  # Any run still going means the answer isn't in yet — a queued check-run can
  # carry a null started_at, which jq sorts *first*, so ordering alone would
  # let an older success mask it. Only once every run has completed is the
  # newest by id (monotonic, unlike timestamps) the one that decides.
  commit="$(git rev-parse origin/main)"
  check="$(gh api "repos/{owner}/{repo}/commits/$commit/check-runs" --jq \
    '[.check_runs[] | select(.name == "test")]
     | if length == 0 then "missing"
       elif any(.status != "completed") then "pending"
       else (sort_by(.id) | last | .conclusion) end')"
  if [ "$check" != "success" ]; then
    echo "error: 'test' on origin/main ($commit) is '$check', refusing to tag" >&2
    exit 1
  fi

  git tag "v$version" "$commit"
  git push --quiet origin "v$version"
  echo "Tagged v$version. The release workflow is building binaries; it will"
  echo "open a draft release for you to write notes on and publish."

# Run the test suite.
test:
  cargo test

# Install git pre-commit hooks (clippy + fmt check).
tools:
  #!/usr/bin/env sh
  hook="$(git rev-parse --git-dir)/hooks/pre-commit"
  printf '%s\n' '#!/usr/bin/env sh' 'set -e' 'cargo fmt --check' 'cargo clippy -- -D warnings' > "$hook"
  chmod +x "$hook"
  echo "Installed pre-commit hook."

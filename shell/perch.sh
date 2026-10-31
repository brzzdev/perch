# perch shell integration (bash/zsh)
#
# Wraps the `perch` binary so that worktree create / switch operations
# `cd` the parent shell into the target directory. The binary prints the
# target path on stdout only when a `cd` hand-off is wanted; everything else
# (prompts, status, errors) is written to stderr.
#
# `perch shell install` loads it from your shell rc with:
#   eval "$(perch shell init zsh)"   # or bash
#
# `br` and `wt` are defined alongside `perch` as shorthand for `perch br` and
# `perch wt`. Set PERCH_NO_SHORTCUTS to any non-empty value before loading it to
# leave both names alone — broot defines its own `br`, and only one can win:
#   PERCH_NO_SHORTCUTS=1
#   eval "$(perch shell init zsh)"
#
# Behaviour:
#   - empty stdout              → nothing to do
#   - single-line existing dir  → cd there
#   - anything else             → print stdout through unchanged

perch() {
  local out
  out="$(command perch "$@")"
  local rc=$?
  if [ -z "$out" ]; then
    return $rc
  fi
  case "$out" in
    *$'\n'*)
      printf '%s\n' "$out"
      ;;
    *)
      if [ -d "$out" ]; then
        cd -- "$out" || return $?
      else
        printf '%s\n' "$out"
      fi
      ;;
  esac
  return $rc
}

# `:-` rather than a bare expansion: an rc running under `set -u` would abort
# here on the far more common case of the variable never having been set.
if [ -z "${PERCH_NO_SHORTCUTS:-}" ]; then
  br() {
    perch br "$@"
  }

  wt() {
    perch wt "$@"
  }

  # zsh completions are claimed by name at compinit time, so `_perch` cannot ask
  # for these two without taking them from whatever else answers to them. Asking
  # here instead ties the claim to the same condition that creates the functions.
  # It needs compinit to have run already; loaded earlier than that, the guard
  # skips it and `br`/`wt` go uncompleted rather than erroring out of your rc.
  if [ -n "${ZSH_VERSION:-}" ] && command -v compdef >/dev/null 2>&1; then
    compdef _perch br wt
  fi
fi

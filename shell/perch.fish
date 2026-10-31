# perch shell integration (fish)
#
# `perch shell install` loads it from ~/.config/fish/conf.d/perch.fish with:
#   perch shell init fish | source
#
# Behaviour mirrors the bash/zsh wrapper: the binary prints the target path on
# stdout only when a cd hand-off is wanted; everything else is on stderr.
#
# `br` and `wt` are defined alongside `perch` as shorthand for `perch br` and
# `perch wt`. Set PERCH_NO_SHORTCUTS to any non-empty value before loading it to
# leave both names alone — broot defines its own `br`, and only one can win:
#   set -gx PERCH_NO_SHORTCUTS 1
#   perch shell init fish | source

function perch
    set -l out (command perch $argv)
    set -l rc $status
    if test -z "$out"
        return $rc
    end
    if test (count $out) -gt 1
        printf '%s\n' $out
        return $rc
    end
    if test -d "$out"
        cd -- "$out"
        or return $status
    else
        printf '%s\n' "$out"
    end
    return $rc
end

if test -z "$PERCH_NO_SHORTCUTS"
    function br
        perch br $argv
    end

    function wt
        perch wt $argv
    end
end

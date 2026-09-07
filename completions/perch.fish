# What the command will accept at the position given — asked of the binary,
# which answers for the position it is handed: worktree names after `wt rm`,
# and everywhere else the branches, minus the words its own match arm eats
# first. It sees remote-only branches, which the `git branch` this replaced
# could not. `command` skips the shell wrapper: it is a function here, and it
# would `cd` the interactive shell on a single-line answer.
function __perch_offers
    command perch $argv --complete 2>/dev/null
end

# True where `--` has just been typed and the dispatcher still has a branch left
# to read: `perch --`, `perch br --`, `perch wt --`. Past `perch wt ls`, or a
# branch the dispatcher has already taken, the words after `--` go nowhere.
function __perch_after_double_dash
    set -l tokens (commandline -opc)
    test "$tokens[-1]" = "--"; or return 1
    test (count $tokens) -eq 2; and return 0
    if test (count $tokens) -eq 3
        contains -- $tokens[2] br wt; and return 0
        test "$tokens[1]" = wt; and test "$tokens[2]" = --no-switch; and return 0
    end
    if test (count $tokens) -eq 4
        test "$tokens[2]" = wt; and test "$tokens[3]" = --no-switch; and return 0
    end
    return 1
end

# True while either removal subverb still wants a target. For `wt rm`, `--`
# closes option parsing and the following word is the target even when it begins
# with `-`. `br rm` keeps its existing long-option grammar.
function __perch_rm_wants_target
    set -l verb $argv[1]
    set -l tokens (commandline -opc)
    set -l reads_options 1
    test (count $tokens) -ge 3; or return 1
    test "$tokens[2]" = $verb; and test "$tokens[3]" = rm; or return 1
    if test (count $tokens) -gt 3
        for token in $tokens[4..-1]
            if test "$token" = --
                if test $verb = wt; and test $reads_options -eq 1
                    set reads_options 0
                else
                    return 1
                end
            else if test $reads_options -eq 1; and string match --quiet -- '-*' $token
                continue
            else
                return 1
            end
        end
    end
    return 0
end

function __perch_in_br_rm
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 3; or return 1
    test "$tokens[2]" = br; and test "$tokens[3]" = rm
end

# True while the command is in `wt`'s option-parsing region. Unlike
# `__fish_seen_subcommand_from`, this checks the verb's position, so a branch
# called `wt` under `br` cannot make the option appear. A regular branch may
# come before the option; a subverb or `--` closes the position.
function __perch_wt_accepts_no_switch
    set -l tokens (commandline -opc)
    set -l first_arg
    switch "$tokens[1]"
        case perch
            test (count $tokens) -ge 2; and test "$tokens[2]" = wt; or return 1
            set first_arg 3
        case wt
            set first_arg 2
        case '*'
            return 1
    end

    test (count $tokens) -ge $first_arg; or return 0
    for token in $tokens[$first_arg..-1]
        test "$token" = --; and return 1
        test "$token" = --no-switch; and return 1
        string match --quiet -- '-*' "$token"; and continue
        contains -- "$token" ls rm list remove; and return 1
    end
    return 0
end

function __perch_wt_wants_branch
    set -l tokens (commandline -opc)
    set -l first_arg
    switch "$tokens[1]"
        case perch
            test (count $tokens) -ge 2; and test "$tokens[2]" = wt; or return 1
            set first_arg 3
        case wt
            set first_arg 2
        case '*'
            return 1
    end

    set -l reads_options 1
    set -l positionals 0
    if test (count $tokens) -ge $first_arg
        for token in $tokens[$first_arg..-1]
            if test $reads_options -eq 1; and test "$token" = --
                set reads_options 0
            else if test $reads_options -eq 1; and test "$token" = --no-switch
                continue
            else if test $reads_options -eq 1; and test $positionals -eq 0; and contains -- "$token" ls rm list remove
                return 1
            else
                set positionals (math $positionals + 1)
            end
        end
    end
    test $positionals -le 1
end

function __perch_wt_branch_offers
    set -l tokens (commandline -opc)
    if test "$tokens[1]" = wt
        command perch wt $tokens[2..-1] --complete 2>/dev/null
    else
        command perch $tokens[2..-1] --complete 2>/dev/null
    end
end

# Top-level: subcommands + the branches reachable without `--`.
complete -c perch -f -n '__fish_is_nth_token 1' -a '(__perch_offers)'
complete -c perch -f -n '__fish_is_nth_token 1' -a 'br' -d 'Check a branch out here'
complete -c perch -f -n '__fish_is_nth_token 1' -a 'wt' -d 'Worktree commands'

# After a `--` that still has a branch to escape: branches, unfiltered. The `--`
# is the position, and it eats nothing at any of the three levels, so one
# question answers for all of them. `wt rm --` is the one escaped route this
# misses, and its own rule below takes it.
complete -c perch -f -n '__perch_after_double_dash' -a '(__perch_offers --)'

# After `br`: its subverb and branches. Unlike bash and zsh, fish has no
# fall-through case, so every verb needs its own rule or the second token
# completes to nothing. `perch br wt` reaches a branch `wt`, while a branch
# named `rm` needs `perch br -- rm`.
complete -c perch -f -n '__fish_seen_subcommand_from br; and __fish_is_nth_token 2; and not __perch_after_double_dash' -a 'rm' -d 'Remove local branches'
complete -c perch -f -n '__fish_seen_subcommand_from br; and __fish_is_nth_token 2' -a '(__perch_offers br)'

# After `wt`: subverbs + the branches reachable without `--`. `__fish_is_nth_token`
# looks past a `--`, so without the guard `perch wt -- ` would offer the subverbs
# on top of the branches the rule above already gave it — and `--` is precisely
# how you say you meant the branch.
complete -c perch -f -n '__fish_seen_subcommand_from wt; and __fish_is_nth_token 2; and not __perch_after_double_dash' -a 'ls rm'
complete -c perch -f -n '__perch_wt_wants_branch' -a '(__perch_wt_branch_offers)'
complete -c perch -f -l no-switch -d 'Create or find the worktree without switching to it' -n '__perch_wt_accepts_no_switch'

# After `wt rm`: the worktrees, until one has been taken.
complete -c perch -f -n '__perch_rm_wants_target wt' -a '(__perch_offers wt rm)'

# After `br rm`: local branches until a target has been taken; flags remain
# valid on either side of it.
complete -c perch -f -n '__perch_rm_wants_target br' -a '(__perch_offers br rm)'
complete -c perch -f -n '__perch_in_br_rm' -l upstream -d 'Also remove explicitly configured same-named upstreams'
complete -c perch -f -n '__perch_in_br_rm' -l force -d 'Skip destructive confirmations'

# `br` and `wt` are the shell wrapper's shorthand for `perch br` and `perch wt`.
# `--wraps` takes a command prefix, so every rule above applies to them at the
# right offset without being restated.
#
# Claim a shortcut name only while it is still ours. Nothing about perch's own
# state can answer that: the completions install without the wrapper, and a `br`
# the wrapper did define can be replaced afterwards by anything sourced later —
# broot ships one. So ask fish what the name resolves to *now*. This file is
# autoloaded on first use, well after config has finished, so the answer here is
# the current one. The body line is matched whole: a stale `--wraps` annotation
# left on someone else's function mentions `perch br` in its signature.
function __perch_owns
    functions $argv[1] 2>/dev/null | string match -qr "^\s*perch $argv[1] \\\$argv\s*\$"
end

if __perch_owns br
    complete -c br --wraps 'perch br'
end
if __perch_owns wt
    complete -c wt --wraps 'perch wt'
end

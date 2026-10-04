# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_cb_global_optspecs
    string join \n n/name= h/help V/version
end

function __fish_cb_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_cb_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_cb_using_subcommand
    set -l cmd (__fish_cb_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c cb -n "__fish_cb_needs_command" -s n -l name -d 'Clipboard to use, instead of the default' -r
complete -c cb -n "__fish_cb_needs_command" -s h -l help -d 'Print help'
complete -c cb -n "__fish_cb_needs_command" -s V -l version -d 'Print version'
complete -c cb -n "__fish_cb_needs_command" -f -a "copy" -d 'Copy files into the clipboard, leaving the originals in place'
complete -c cb -n "__fish_cb_needs_command" -f -a "cut" -d 'Record files to be moved when pasted. The originals stay in place until then'
complete -c cb -n "__fish_cb_needs_command" -f -a "paste" -d 'Write the clipboard\'s files into the current directory'
complete -c cb -n "__fish_cb_needs_command" -f -a "list" -d 'List what the clipboard holds'
complete -c cb -n "__fish_cb_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c cb -n "__fish_cb_using_subcommand copy" -s n -l name -d 'Clipboard to use, instead of the default' -r
complete -c cb -n "__fish_cb_using_subcommand copy" -s h -l help -d 'Print help'
complete -c cb -n "__fish_cb_using_subcommand cut" -s n -l name -d 'Clipboard to use, instead of the default' -r
complete -c cb -n "__fish_cb_using_subcommand cut" -s h -l help -d 'Print help'
complete -c cb -n "__fish_cb_using_subcommand paste" -s d -l directory -d 'Destination directory, defaults to the current one' -r -F
complete -c cb -n "__fish_cb_using_subcommand paste" -l on-conflict -d 'What to do when a destination file already exists' -r
complete -c cb -n "__fish_cb_using_subcommand paste" -s n -l name -d 'Clipboard to use, instead of the default' -r
complete -c cb -n "__fish_cb_using_subcommand paste" -s h -l help -d 'Print help'
complete -c cb -n "__fish_cb_using_subcommand list" -s n -l name -d 'Clipboard to use, instead of the default' -r
complete -c cb -n "__fish_cb_using_subcommand list" -s h -l help -d 'Print help'
complete -c cb -n "__fish_cb_using_subcommand help; and not __fish_seen_subcommand_from copy cut paste list help" -f -a "copy" -d 'Copy files into the clipboard, leaving the originals in place'
complete -c cb -n "__fish_cb_using_subcommand help; and not __fish_seen_subcommand_from copy cut paste list help" -f -a "cut" -d 'Record files to be moved when pasted. The originals stay in place until then'
complete -c cb -n "__fish_cb_using_subcommand help; and not __fish_seen_subcommand_from copy cut paste list help" -f -a "paste" -d 'Write the clipboard\'s files into the current directory'
complete -c cb -n "__fish_cb_using_subcommand help; and not __fish_seen_subcommand_from copy cut paste list help" -f -a "list" -d 'List what the clipboard holds'
complete -c cb -n "__fish_cb_using_subcommand help; and not __fish_seen_subcommand_from copy cut paste list help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'

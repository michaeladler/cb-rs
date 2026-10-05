
use builtin;
use str;

set edit:completion:arg-completer[cb] = {|@words|
    fn spaces {|n|
        builtin:repeat $n ' ' | str:join ''
    }
    fn cand {|text desc|
        edit:complex-candidate $text &display=$text' '(spaces (- 14 (wcswidth $text)))$desc
    }
    var command = 'cb'
    for word $words[1..-1] {
        if (str:has-prefix $word '-') {
            break
        }
        set command = $command';'$word
    }
    var completions = [
        &'cb'= {
            cand -n 'Clipboard to use, instead of the default'
            cand --name 'Clipboard to use, instead of the default'
            cand -h 'Print help'
            cand --help 'Print help'
            cand -V 'Print version'
            cand --version 'Print version'
            cand copy 'Record files to be copied when pasted, leaving the originals in place'
            cand cut 'Record files to be moved when pasted. The originals stay in place until then'
            cand paste 'Write the clipboard''s files into the current directory'
            cand list 'List what the clipboard holds'
            cand help 'Print this message or the help of the given subcommand(s)'
        }
        &'cb;copy'= {
            cand -n 'Clipboard to use, instead of the default'
            cand --name 'Clipboard to use, instead of the default'
            cand -a 'add to the recorded paths instead of replacing them'
            cand --amend 'Add to the recorded paths instead of replacing them'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'cb;cut'= {
            cand -n 'Clipboard to use, instead of the default'
            cand --name 'Clipboard to use, instead of the default'
            cand -a 'add to the recorded paths instead of replacing them'
            cand --amend 'Add to the recorded paths instead of replacing them'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'cb;paste'= {
            cand -d 'Destination directory, defaults to the current one'
            cand --directory 'Destination directory, defaults to the current one'
            cand --on-conflict 'What to do when a destination file already exists'
            cand -n 'Clipboard to use, instead of the default'
            cand --name 'Clipboard to use, instead of the default'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'cb;list'= {
            cand -n 'Clipboard to use, instead of the default'
            cand --name 'Clipboard to use, instead of the default'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'cb;help'= {
            cand copy 'Record files to be copied when pasted, leaving the originals in place'
            cand cut 'Record files to be moved when pasted. The originals stay in place until then'
            cand paste 'Write the clipboard''s files into the current directory'
            cand list 'List what the clipboard holds'
            cand help 'Print this message or the help of the given subcommand(s)'
        }
        &'cb;help;copy'= {
        }
        &'cb;help;cut'= {
        }
        &'cb;help;paste'= {
        }
        &'cb;help;list'= {
        }
        &'cb;help;help'= {
        }
    ]
    $completions[$command]
}

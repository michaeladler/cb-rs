
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'cb' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'cb'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'cb' {
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('copy', 'copy', [CompletionResultType]::ParameterValue, 'Copy files into the clipboard, leaving the originals in place')
            [CompletionResult]::new('cut', 'cut', [CompletionResultType]::ParameterValue, 'Record files to be moved when pasted. The originals stay in place until then')
            [CompletionResult]::new('paste', 'paste', [CompletionResultType]::ParameterValue, 'Write the clipboard''s files into the current directory')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List what the clipboard holds')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'cb;copy' {
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'cb;cut' {
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'cb;paste' {
            [CompletionResult]::new('-d', '-d', [CompletionResultType]::ParameterName, 'Destination directory, defaults to the current one')
            [CompletionResult]::new('--directory', '--directory', [CompletionResultType]::ParameterName, 'Destination directory, defaults to the current one')
            [CompletionResult]::new('--on-conflict', '--on-conflict', [CompletionResultType]::ParameterName, 'What to do when a destination file already exists')
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'cb;list' {
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Clipboard to use, instead of the default')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'cb;help' {
            [CompletionResult]::new('copy', 'copy', [CompletionResultType]::ParameterValue, 'Copy files into the clipboard, leaving the originals in place')
            [CompletionResult]::new('cut', 'cut', [CompletionResultType]::ParameterValue, 'Record files to be moved when pasted. The originals stay in place until then')
            [CompletionResult]::new('paste', 'paste', [CompletionResultType]::ParameterValue, 'Write the clipboard''s files into the current directory')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List what the clipboard holds')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'cb;help;copy' {
            break
        }
        'cb;help;cut' {
            break
        }
        'cb;help;paste' {
            break
        }
        'cb;help;list' {
            break
        }
        'cb;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}

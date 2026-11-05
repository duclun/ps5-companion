# Forward arguments verbatim to the separate homebrew catalog CLI.
$pythonCommand = Get-Command python.exe -ErrorAction SilentlyContinue
if (-not $pythonCommand) {
    [Console]::Error.WriteLine('Python 3 is required.')
    exit 1
}
$previousHomebrewEncoding = $env:PYTHONIOENCODING
try {
    $env:PYTHONIOENCODING = 'utf-8'
    & $pythonCommand.Source (Join-Path $PSScriptRoot 'scripts\ps5_homebrew.py') @args
    $homebrewExitCode = $LASTEXITCODE
} finally {
    if ($null -eq $previousHomebrewEncoding) { Remove-Item Env:PYTHONIOENCODING -ErrorAction SilentlyContinue }
    else { $env:PYTHONIOENCODING = $previousHomebrewEncoding }
}
exit $homebrewExitCode

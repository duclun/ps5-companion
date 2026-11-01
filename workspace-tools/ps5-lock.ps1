param(
    [Parameter(Mandatory=$true,Position=0)][ValidateSet('diagnose','recover')][string] $Command,
    [Parameter(Mandatory=$true,Position=1)][ValidatePattern('^(PPSA|CUSA)[0-9]{5}$')][string] $TitleId,
    [string] $Out
)
$ErrorActionPreference = 'Stop'
$lockArgs = @((Join-Path $PSScriptRoot 'scripts\ps5_lock.py'), $Command, $TitleId)
if ($Out) { $lockArgs += @('--out', $Out) }
& (Get-Command python.exe -ErrorAction Stop).Source @lockArgs
exit $LASTEXITCODE

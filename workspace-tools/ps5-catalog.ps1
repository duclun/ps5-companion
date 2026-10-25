param(
    [ValidateSet('refresh', 'build')][string] $Command = 'refresh',
    [ValidateSet('all', 'blackbox', 'orbit')][string] $Store = 'all'
)
$ErrorActionPreference = 'Stop'
$python = Join-Path $PSScriptRoot '.tools\catalog-python\Scripts\python.exe'
if (-not (Test-Path -LiteralPath $python -PathType Leaf)) { $python = (Get-Command python.exe -ErrorAction Stop).Source }
& $python (Join-Path $PSScriptRoot 'scripts\ps5_catalog.py') $Command $Store
exit $LASTEXITCODE

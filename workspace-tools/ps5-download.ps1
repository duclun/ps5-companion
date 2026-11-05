param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^(PPSA|CUSA)[0-9]{5}$')]
    [string] $TitleId,
    [switch] $List,
    [switch] $NoBrowserPrompt,
    [switch] $DownloadOnly,
    [switch] $PackageOnly,
    [ValidateRange(1, 10000)]
    [int] $SourceNumber,
    [switch] $IncludeMediaFire,
    [ValidatePattern('^(PPSA|CUSA)[0-9]{5}$')]
    [string] $ArtifactTitleId,
    [string] $CatalogPath,
    [string] $Addr = '192.168.1.214',
    [string] $Engine = 'http://127.0.0.1:19113'
)

$ErrorActionPreference = 'Stop'
if ($TitleId -like 'CUSA*' -and -not $PackageOnly) { throw 'PS4 download requires -PackageOnly.' }
if ($SourceNumber -and -not $PackageOnly) { throw '-SourceNumber requires -PackageOnly.' }
$releaseCatalogPath = $CatalogPath
if ([string]::IsNullOrWhiteSpace($releaseCatalogPath)) {
    $catalogOutput = @(& (Get-Command python.exe -ErrorAction Stop).Source (Join-Path $PSScriptRoot 'scripts\ps5_catalog.py') build)
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($catalogOutput.Count -ne 1) { throw 'Catalog builder did not return one snapshot path.' }
    $releaseCatalogPath = Join-Path $catalogOutput[0] 'releases'
}
$outputPath = Join-Path $PSScriptRoot ('downloads\' + $TitleId.ToUpperInvariant())

$pythonCommand = Get-Command python.exe -ErrorAction SilentlyContinue
if (-not $pythonCommand) {
    [Console]::Error.WriteLine('Python 3 is required for the download workflow.')
    exit 1
}
if (-not (Test-Path -LiteralPath (Join-Path $releaseCatalogPath ($TitleId.ToUpperInvariant() + '.json')) -PathType Leaf)) {
    [Console]::Error.WriteLine("No local release catalog found for $TitleId in $releaseCatalogPath")
    exit 1
}

$downloadArguments = @((Join-Path $PSScriptRoot 'scripts\ps5_download.py'), $TitleId.ToUpperInvariant(), '--catalog', $releaseCatalogPath, '--out', $outputPath)
if ($List) { $downloadArguments += '--list' }
elseif (-not $NoBrowserPrompt) { $downloadArguments += '--interactive' }
if (-not $List -and -not $DownloadOnly -and -not $PackageOnly) { $downloadArguments += @('--through-upload', '--addr', $Addr, '--engine', $Engine) }
if ($PackageOnly) { $downloadArguments += '--package-only' }
if ($SourceNumber) { $downloadArguments += @('--source-number', [string]$SourceNumber) }
if ($IncludeMediaFire) { $downloadArguments += '--include-mediafire' }
if ($ArtifactTitleId) { $downloadArguments += @('--artifact-title-id', $ArtifactTitleId.ToUpperInvariant()) }
$previousDownloadEncoding = $env:PYTHONIOENCODING
try {
    $env:PYTHONIOENCODING = 'utf-8'
    & $pythonCommand.Source @downloadArguments
    $downloadExitCode = $LASTEXITCODE
} finally {
    if ($null -eq $previousDownloadEncoding) { Remove-Item Env:PYTHONIOENCODING -ErrorAction SilentlyContinue }
    else { $env:PYTHONIOENCODING = $previousDownloadEncoding }
}
exit $downloadExitCode

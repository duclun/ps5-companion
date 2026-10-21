param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^PPSA[A-Za-z0-9]+$')]
    [string] $TitleId,
    [switch] $List,
    [switch] $NoBrowserPrompt,
    [switch] $DownloadOnly,
    [switch] $IncludeMediaFire,
    [string] $Addr = '192.168.1.214',
    [string] $Engine = 'http://127.0.0.1:19113'
)

$ErrorActionPreference = 'Stop'
$catalogPath = Join-Path $PSScriptRoot 'repos\overdrive\app\data\releases'
$outputPath = Join-Path $PSScriptRoot ('downloads\' + $TitleId.ToUpperInvariant())

$pythonCommand = Get-Command python.exe -ErrorAction SilentlyContinue
if (-not $pythonCommand) {
    [Console]::Error.WriteLine('Python 3 is required for the download workflow.')
    exit 1
}
if (-not (Test-Path -LiteralPath (Join-Path $catalogPath ($TitleId.ToUpperInvariant() + '.json')) -PathType Leaf)) {
    [Console]::Error.WriteLine("No local release catalog found for $TitleId in $catalogPath")
    exit 1
}

$downloadArguments = @((Join-Path $PSScriptRoot 'scripts\ps5_download.py'), $TitleId.ToUpperInvariant(), '--catalog', $catalogPath, '--out', $outputPath)
if ($List) { $downloadArguments += '--list' }
elseif (-not $NoBrowserPrompt) { $downloadArguments += '--interactive' }
if (-not $List -and -not $DownloadOnly) { $downloadArguments += @('--through-upload', '--addr', $Addr, '--engine', $Engine) }
if ($IncludeMediaFire) { $downloadArguments += '--include-mediafire' }
& $pythonCommand.Source @downloadArguments
exit $LASTEXITCODE

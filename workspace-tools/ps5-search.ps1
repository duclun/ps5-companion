param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Name
)

$ErrorActionPreference = 'Stop'
$query = ($Name -join ' ').Trim()
if ([string]::IsNullOrWhiteSpace($query)) {
    [Console]::Error.WriteLine('Usage: .\ps5-search "outer wild"')
    exit 2
}

$cliPath = Join-Path $PSScriptRoot 'repos\ps5-companion\target\debug\ps5-companion.exe'
$catalogPath = Join-Path $PSScriptRoot 'repos\overdrive\app\data'

if (-not (Test-Path -LiteralPath $cliPath -PathType Leaf)) {
    [Console]::Error.WriteLine("PS5 companion executable not found: $cliPath")
    exit 1
}
if (-not (Test-Path -LiteralPath $catalogPath -PathType Container)) {
    [Console]::Error.WriteLine("Overdrive catalog directory not found: $catalogPath")
    exit 1
}

$output = @(& $cliPath search $query --catalog-dir $catalogPath)
$searchExitCode = $LASTEXITCODE
$output | ForEach-Object { Write-Host $_ }
if ($searchExitCode -ne 0) { exit $searchExitCode }

$games = @(
    foreach ($line in $output) {
        if ($line -match '^\s+(?<id>PPSA[0-9]{5})\s+(?<title>.+?)\s+\[(?<edition>[^\]]+)\]') {
            [pscustomobject]@{ TitleId = $Matches.id; Title = $Matches.title.Trim(); Edition = $Matches.edition }
        }
    }
)
if ($games.Count -eq 0) { exit 0 }

$selected = $games[0]
if ($games.Count -gt 1) {
    Write-Host ''
    for ($index = 0; $index -lt $games.Count; $index++) {
        Write-Host ('  {0}. {1} ({2}) [{3}]' -f ($index + 1), $games[$index].Title, $games[$index].TitleId, $games[$index].Edition)
    }
    while ($true) {
        $choice = Read-Host 'Select a game number, or press Enter to cancel'
        if ([string]::IsNullOrWhiteSpace($choice)) { exit 0 }
        $number = 0
        if ([int]::TryParse($choice, [ref]$number) -and $number -ge 1 -and $number -le $games.Count) {
            $selected = $games[$number - 1]
            break
        }
        Write-Host 'Enter one of the listed numbers.'
    }
}

$answer = Read-Host ('Download, inspect, extract and upload {0} ({1}) to PS5? [y/N]' -f $selected.Title, $selected.TitleId)
if ($answer.Trim() -notmatch '^(?i:y|yes)$') { exit 0 }

& (Join-Path $PSScriptRoot 'ps5-download.ps1') -TitleId $selected.TitleId
exit $LASTEXITCODE

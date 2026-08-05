[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$CookieFile,

    [string]$DataDirectory = 'E:\osudata',

    [Parameter(Mandatory, ValueFromRemainingArguments)]
    [string[]]$PackIds,

    [ValidateRange(1, 32)]
    [int]$DownloadConcurrency = 2
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataPath = [System.IO.Path]::GetFullPath($DataDirectory)
$cookiePath = (Resolve-Path -LiteralPath $CookieFile).Path
$binary = Join-Path $projectRoot 'target\release\osu-difficulty-lab.exe'

if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "Release binary was not found: $binary. Run cargo build --release first."
}

Push-Location $projectRoot
try {
    # Per-pack failures are status output, not a failure of the whole retry run.
    $downloadErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $downloadOutput = [System.Collections.Generic.List[string]]::new()
        & $binary download-packs $dataPath --cookie-file $cookiePath --concurrency $DownloadConcurrency $PackIds 2>&1 | ForEach-Object {
            $line = $_.ToString()
            $downloadOutput.Add($line)
            Write-Host $line
        }
        $downloadExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $downloadErrorActionPreference
    }
    if ($downloadExitCode -ne 0) { throw "download-packs exited with $downloadExitCode" }

    $downloaded = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $skipped = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($line in $downloadOutput) {
        if ($line.ToString() -match '^([A-Z]+\d+): (downloaded|already complete)$') {
            [void]$downloaded.Add($Matches[1])
        }
        elseif ($line.ToString() -match '^([A-Z]+\d+): skipped ') {
            [void]$skipped.Add($Matches[1])
        }
    }

    $failed = [System.Collections.Generic.List[string]]::new()
    foreach ($packId in $PackIds) {
        if ($skipped.Contains($packId)) {
            continue
        }
        if (-not $downloaded.Contains($packId)) {
            $failed.Add($packId)
            continue
        }
        try {
            & $binary ingest-downloaded $dataPath $packId
            if ($LASTEXITCODE -ne 0) { throw "ingest-downloaded exited with $LASTEXITCODE" }
        }
        catch {
            $failed.Add($packId)
            Write-Warning $_.Exception.Message
        }
    }

    if ($failed.Count -gt 0) {
        $failedPath = Join-Path $dataPath 'failed-pack-ids.txt'
        Set-Content -LiteralPath $failedPath -Value ($failed | Sort-Object -Unique)
        throw ("{0} packs remain failed; IDs were saved to {1}" -f $failed.Count, $failedPath)
    }
}
finally {
    Pop-Location
}

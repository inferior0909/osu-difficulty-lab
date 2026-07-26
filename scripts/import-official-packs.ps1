[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$CookieFile,

    [string]$DataDirectory = (Join-Path $PSScriptRoot "..\data"),

    [ValidateRange(1, 100)]
    [int]$BatchSize = 1,

    [switch]$RefreshCatalog,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataPath = [System.IO.Path]::GetFullPath($DataDirectory)
$cookiePath = (Resolve-Path -LiteralPath $CookieFile).Path
$catalogPath = Join-Path $dataPath 'official-pack-ids.txt'
$failedPath = Join-Path $dataPath 'failed-pack-ids.txt'

New-Item -ItemType Directory -Force -Path $dataPath | Out-Null

Push-Location $projectRoot
try {
    if ($Release) {
        cargo build --release
        $binary = Join-Path $projectRoot 'target\release\osu-difficulty-lab.exe'
    }
    else {
        cargo build
        $binary = Join-Path $projectRoot 'target\debug\osu-difficulty-lab.exe'
    }

    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "CLI binary was not produced: $binary"
    }

    & $binary init $dataPath
    if ($LASTEXITCODE -ne 0) { throw "init exited with $LASTEXITCODE" }

    if ($RefreshCatalog -or -not (Test-Path -LiteralPath $catalogPath -PathType Leaf)) {
        & $binary catalog-sync --output $catalogPath
        if ($LASTEXITCODE -ne 0) { throw "catalog-sync exited with $LASTEXITCODE" }
    }

    $packIds = @(Get-Content -LiteralPath $catalogPath | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($packIds.Count -eq 0) { throw 'The official pack catalogue is empty.' }

    $failed = [System.Collections.Generic.List[string]]::new()
    for ($offset = 0; $offset -lt $packIds.Count; $offset += $BatchSize) {
        $last = [Math]::Min($offset + $BatchSize - 1, $packIds.Count - 1)
        $batch = @($packIds[$offset..$last])
        Write-Host ("[{0}/{1}] {2}" -f ($offset + 1), $packIds.Count, ($batch -join ', '))
        try {
            # Rust processes a batch sequentially, retaining only one temporary .part archive.
            & $binary ingest-packs $dataPath --cookie-file $cookiePath $batch
            if ($LASTEXITCODE -ne 0) { throw "ingest-packs exited with $LASTEXITCODE" }
        }
        catch {
            $failed.AddRange([string[]]$batch)
            Write-Warning $_.Exception.Message
        }
    }

    # Freeze global quantiles only after raw features from this run exist; then build the primary HNSW index.
    & $binary normalizer-fit $dataPath --version 1
    if ($LASTEXITCODE -ne 0) { throw "normalizer-fit exited with $LASTEXITCODE" }
    & $binary index-build $dataPath --version 1
    if ($LASTEXITCODE -ne 0) { throw "index-build exited with $LASTEXITCODE" }
    & $binary doctor $dataPath --version 1
    if ($LASTEXITCODE -ne 0) { throw "doctor exited with $LASTEXITCODE" }

    if ($failed.Count -gt 0) {
        Set-Content -LiteralPath $failedPath -Value ($failed | Sort-Object -Unique)
        throw ("{0} packs failed. IDs were saved to {1}; successful packs are indexed." -f $failed.Count, $failedPath)
    }
}
finally {
    Pop-Location
}

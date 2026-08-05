[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$CookieFile,

    # This folder is the persistent research database: SQLite, feature indexes, and
    # the original `.osu` files. Archives and all non-`.osu` assets are removed.
    [string]$DataDirectory = 'E:\osudata',

    # Retained so existing command lines continue to work. Imports now run one
    # pack at a time, so this setting has no effect.
    [ValidateRange(1, 100)]
    [int]$BatchSize = 1,

    # Retained for backwards compatibility. Sequential mode always uses one
    # downloader, allowing a single clear progress bar in the terminal.
    [ValidateRange(1, 32)]
    [int]$DownloadConcurrency = 1,

    [ValidateSet('Official', 'Hinamizawa')]
    [string]$DownloadSource = 'Hinamizawa',

    # Optional HTTP(S) or SOCKS5 proxy, for example http://127.0.0.1:7890.
    [string]$Proxy,

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
$stagingPath = Join-Path $dataPath 'tmp'
$stagedArchives = if (Test-Path -LiteralPath $stagingPath -PathType Container) {
    @(Get-ChildItem -LiteralPath $stagingPath -Filter '*.part' -File -ErrorAction SilentlyContinue)
}
else {
    @()
}
$stagedBytes = ($stagedArchives | Measure-Object -Property Length -Sum).Sum
if ($null -eq $stagedBytes) { $stagedBytes = 0 }
Write-Host (
    "Data directory: {0} | found {1} staged .part archive(s), {2:N1} GiB; downloads will resume when supported." -f
    $dataPath, $stagedArchives.Count, ($stagedBytes / 1GB)
)

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
    & $binary validate-cookie --cookie-file $cookiePath
    if ($LASTEXITCODE -ne 0) { throw "validate-cookie exited with $LASTEXITCODE" }

    if ($RefreshCatalog -or -not (Test-Path -LiteralPath $catalogPath -PathType Leaf)) {
        $catalogArgs = @('catalog-sync', '--output', $catalogPath)
        if ($Proxy) { $catalogArgs += @('--proxy', $Proxy) }
        & $binary @catalogArgs
        if ($LASTEXITCODE -ne 0) { throw "catalog-sync exited with $LASTEXITCODE" }
    }

    # Keep all catalog entries. The downloader rejects known non-standard packs
    # by ID and retains every mixed pack, so a pack containing standard charts
    # is never omitted before its contents can be checked.
    $packIds = @(Get-Content -LiteralPath $catalogPath | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($packIds.Count -eq 0) { throw 'The official pack catalogue is empty.' }

    Write-Host ("Sequential mode: downloading and importing one pack at a time via {0}." -f $DownloadSource)
    $failed = [System.Collections.Generic.List[string]]::new()
    for ($index = 0; $index -lt $packIds.Count; $index++) {
        $packId = $packIds[$index]
        Write-Host ("[{0}/{1}] {2}" -f ($index + 1), $packIds.Count, $packId)
        try {
            $ingestArgs = @(
                'ingest-packs', $dataPath,
                '--cookie-file', $cookiePath,
                '--source', $DownloadSource.ToLowerInvariant()
            )
            if ($Proxy) { $ingestArgs += @('--proxy', $Proxy) }
            $ingestArgs += $packId
            & $binary @ingestArgs
            if ($LASTEXITCODE -ne 0) { throw "ingest-packs exited with $LASTEXITCODE" }
        }
        catch {
            $failed.Add($packId)
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

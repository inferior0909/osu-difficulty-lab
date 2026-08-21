[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$CookieFile,
    [string]$DataDirectory = 'E:\osu-mania-ranked',
    [switch]$RefreshCatalog,
    [string]$Proxy
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$data = [IO.Path]::GetFullPath($DataDirectory)
$cookiePath = (Resolve-Path $CookieFile).Path
$packsDir = Join-Path $data 'packs'
$mapsDir = Join-Path $data 'beatmaps'
$catalog = Join-Path $data 'mania-pack-ids.txt'
$db = Join-Path $data 'mania-ranked.sqlite'
$schema = Join-Path $PSScriptRoot 'mania-ranked-schema.sql'
New-Item -ItemType Directory -Force -Path $data, $packsDir, $mapsDir | Out-Null

$cookie = foreach ($line in Get-Content $cookiePath) {
    if (-not $line -or $line.StartsWith('!') -or ($line.StartsWith('#') -and -not $line.StartsWith('#HttpOnly_'))) { continue }
    $p = $line -split "`t"; if ($p.Count -lt 7) { continue }
    $domain = if ($p[0].StartsWith('#HttpOnly_')) { $p[0].Substring(10) } else { $p[0] }
    if ($domain -eq 'osu.ppy.sh' -or $domain.EndsWith('.osu.ppy.sh')) { '{0}={1}' -f $p[5], $p[6] }
}
$cookie = @($cookie) -join '; '
if (-not $cookie) { throw 'No osu.ppy.sh cookies found.' }

$sqlite = Get-Command sqlite3 -ErrorAction Stop
& $sqlite.Source $db ".read $schema"
if ($LASTEXITCODE -ne 0) { throw 'SQLite schema initialization failed.' }

if ($RefreshCatalog -or -not (Test-Path $catalog)) {
    Push-Location $root
    try { cargo run --quiet -- catalog-sync --output $catalog } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw 'catalog-sync failed.' }
    Get-Content $catalog | Where-Object { $_ -match '^SM\d+$' } | Set-Content $catalog
}
$packs = @(Get-Content $catalog | Where-Object { $_ -match '^SM\d+$' } | Sort-Object -Unique)
if (-not $packs) { throw 'No SM (mania) packs found in the catalogue.' }

foreach ($pack in $packs) {
    $archive = Join-Path $packsDir "$pack.download"
    $pageArgs = @{ Uri = "https://osu.ppy.sh/beatmaps/packs/$pack"; Headers = @{ Cookie = $cookie }; MaximumRedirection = 5 }
    if ($Proxy) { $pageArgs.Proxy = $Proxy }
    $html = (Invoke-WebRequest @pageArgs).Content
    $match = [regex]::Match($html, 'https://packs\.ppy\.sh/[^"''<> ]+')
    if (-not $match.Success) { Write-Warning "${pack}: download link not found"; continue }
    $url = [System.Net.WebUtility]::HtmlDecode($match.Value)
    if (-not (Test-Path $archive)) {
        $downloadArgs = @{ Uri = $url; Headers = @{ Cookie = $cookie }; OutFile = $archive; MaximumRedirection = 5 }
        if ($Proxy) { $downloadArgs.Proxy = $Proxy }
        Invoke-WebRequest @downloadArgs
    }
    $scratch = Join-Path $packsDir "$pack.tmp"
    Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $scratch | Out-Null
    $ext = [IO.Path]::GetExtension($url).ToLowerInvariant()
    if ($ext -eq '.zip' -or $ext -eq '.osz') { Expand-Archive -LiteralPath $archive -DestinationPath $scratch -Force }
    else {
        $sevenZip = Get-Command 7z -ErrorAction Stop
        & $sevenZip.Source x $archive "-o$scratch" -y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "7z extraction failed for $pack" }
    }
    Get-ChildItem $scratch -Recurse -Filter '*.osu' -File | ForEach-Object {
        $target = Join-Path $mapsDir $_.Name
        Move-Item $_.FullName $target -Force
        $id = [IO.Path]::GetFileNameWithoutExtension($_.Name)
        if ($id -match '^\d+$') {
            & $sqlite.Source $db "INSERT OR IGNORE INTO mania_ranked_beatmaps(beatmap_id,beatmapset_id,artist,title,version,creator,status,mode,downloaded_path,download_status,catalogued_at) VALUES($id,0,'','','','$pack','ranked','mania','$target','downloaded',datetime('now'));"
        }
    }
    Remove-Item $scratch -Recurse -Force
    Write-Host "$pack extracted (.osu only)"
}

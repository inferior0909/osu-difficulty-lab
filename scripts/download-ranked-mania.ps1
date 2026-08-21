[CmdletBinding()]
param(
    # Create an OAuth application at https://osu.ppy.sh/home/account/edit#new-oauth-application.
    [string]$ClientId,

    [string]$ClientSecret,

    # Netscape-format cookie exported from a logged-in osu.ppy.sh browser tab.
    # It is needed to download the individual .osu files.
    [string]$CookieFile,

    [string]$DataDirectory = 'E:\osu-mania-ranked',

    [ValidateRange(1, 10)]
    [int]$DownloadConcurrency = 3,

    [ValidateRange(1, 10)]
    [int]$RetryCount = 3,

    [ValidateRange(1, 1048576)]
    [int]$MinimumBytesPerSecond = 1024,

    [ValidateRange(5, 300)]
    [int]$LowSpeedWindowSeconds = 20,

    [string]$Proxy,

    # Optional full path when sqlite3.exe is not on PATH.
    [string]$SqlitePath,

    # Refresh the catalogue even when a previous catalogue file exists.
    [switch]$RefreshCatalog,

    # Only create the SQLite database/table and exit.
    [switch]$InitializeOnly
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataPath = [System.IO.Path]::GetFullPath($DataDirectory)
$cookiePath = if ($CookieFile) { (Resolve-Path -LiteralPath $CookieFile).Path } else { $null }
$schemaPath = Join-Path $PSScriptRoot 'mania-ranked-schema.sql'
$databasePath = Join-Path $dataPath 'mania-ranked.sqlite'
$catalogPath = Join-Path $dataPath 'mania-ranked-catalog.jsonl'
$manifestPath = Join-Path $dataPath 'mania-ranked-download-manifest.csv'
$failurePath = Join-Path $dataPath 'mania-ranked-failures.csv'
$beatmapPath = Join-Path $dataPath 'beatmaps'

function Get-OsuCookieHeader([string]$Path) {
    $pairs = foreach ($line in Get-Content -LiteralPath $Path) {
        if (-not $line -or $line.StartsWith('!') -or ($line.StartsWith('#') -and -not $line.StartsWith('#HttpOnly_'))) { continue }
        $parts = $line -split "`t"
        if ($parts.Count -lt 7) { continue }
        $domain = if ($parts[0].StartsWith('#HttpOnly_')) { $parts[0].Substring(10) } else { $parts[0] }
    # Browser exports may scope the session cookie to .ppy.sh rather than
    # osu.ppy.sh; both are valid for the osu! host.
    if ($domain -eq 'ppy.sh' -or $domain -eq '.ppy.sh' -or $domain -eq 'osu.ppy.sh' -or $domain.EndsWith('.osu.ppy.sh')) {
            '{0}={1}' -f $parts[5], $parts[6]
        }
    }
    $header = @($pairs) -join '; '
    if (-not $header) { throw "No osu.ppy.sh cookies were found in $Path" }
    return $header
}

function Invoke-OsuRequest([string]$Uri, [hashtable]$Headers = @{}, [string]$OutFile) {
    $args = @{ Uri = $Uri; Headers = $Headers; MaximumRedirection = 5 }
    if ($Proxy) { $args.Proxy = $Proxy }
    if ($OutFile) { $args.OutFile = $OutFile }
    Invoke-WebRequest @args
}

function Get-AccessToken {
    $body = @{ client_id = $ClientId; client_secret = $ClientSecret; grant_type = 'client_credentials'; scope = 'public' }
    $args = @{ Uri = 'https://osu.ppy.sh/oauth/token'; Method = 'Post'; ContentType = 'application/json'; Body = ($body | ConvertTo-Json -Compress) }
    if ($Proxy) { $args.Proxy = $Proxy }
    $response = Invoke-RestMethod @args
    if (-not $response.access_token) { throw 'osu! OAuth did not return an access token.' }
    return $response.access_token
}

function Initialize-Database {
    $sqlite = Resolve-Sqlite
    if (-not $sqlite) {
        Write-Warning 'sqlite3.exe was not found. Downloads will continue, but SQLite table initialization is skipped. Re-run with -SqlitePath after installing SQLite.'
        return $false
    }
    & $sqlite $databasePath ".read $schemaPath"
    if ($LASTEXITCODE -ne 0) { throw "sqlite3 schema initialization failed with exit code $LASTEXITCODE" }
    return $true
}

function Resolve-Sqlite {
    if ($SqlitePath -and (Test-Path -LiteralPath $SqlitePath -PathType Leaf)) { return (Resolve-Path -LiteralPath $SqlitePath).Path }
    $command = Get-Command sqlite3.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    foreach ($candidate in @(
        "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\SQLite.SQLite_*\**\sqlite3.exe",
        "$env:ProgramFiles\SQLite\sqlite3.exe",
        "$env:ProgramFiles\sqlite-tools\sqlite3.exe",
        "$env:ChocolateyInstall\bin\sqlite3.exe"
    )) {
        $found = Get-ChildItem -Path $candidate -File -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

function Import-ManifestIntoDatabase {
    $sqlite = Resolve-Sqlite
    if (-not $sqlite) { throw 'sqlite3.exe is unavailable.' }
    & $sqlite $databasePath 'DELETE FROM mania_ranked_beatmaps;'
    if ($LASTEXITCODE -ne 0) { throw "sqlite3 could not clear the catalogue table (exit code $LASTEXITCODE)" }
    & $sqlite $databasePath ".mode csv`n.import --csv --skip 1 `"$manifestPath`" mania_ranked_beatmaps"
    if ($LASTEXITCODE -ne 0) { throw "sqlite3 could not import the download manifest (exit code $LASTEXITCODE)" }
}

New-Item -ItemType Directory -Force -Path $dataPath, $beatmapPath | Out-Null
$sqliteAvailable = Initialize-Database
if ($InitializeOnly) {
    if (-not $sqliteAvailable) { throw 'Cannot use -InitializeOnly without sqlite3.exe. Install it or pass -SqlitePath.' }
    Write-Host "Initialized $databasePath"
    exit 0
}

if (-not $ClientId -or -not $ClientSecret) { throw 'ClientId and ClientSecret are required unless -InitializeOnly is used.' }
if (-not $cookiePath -or -not (Test-Path -LiteralPath $cookiePath -PathType Leaf)) { throw 'CookieFile is required unless -InitializeOnly is used, and must point to a file.' }

$cookieHeader = Get-OsuCookieHeader $cookiePath
if ($RefreshCatalog -or -not (Test-Path -LiteralPath $catalogPath -PathType Leaf)) {
    $token = Get-AccessToken
    $headers = @{ Authorization = "Bearer $token"; Accept = 'application/json' }
    $cursor = $null
    $rows = [System.Collections.Generic.List[object]]::new()
    do {
        $uri = 'https://osu.ppy.sh/api/v2/beatmapsets/search?m=3&s=ranked'
        if ($cursor) { $uri += '&cursor_string=' + [uri]::EscapeDataString($cursor) }
        $page = (Invoke-OsuRequest -Uri $uri -Headers $headers).Content | ConvertFrom-Json
        foreach ($set in @($page.beatmapsets)) {
            if ($set.status -ne 'ranked') { continue }
            # A ranked set can contain multiple modes and individual difficulties
            # with different statuses. Keep only the actually Ranked mania maps.
            foreach ($map in @($set.beatmaps | Where-Object { $_.mode -eq 'mania' -and $_.status -eq 'ranked' })) {
                $rows.Add([ordered]@{
                    beatmap_id = [int64]$map.id; beatmapset_id = [int64]$set.id; checksum = $map.checksum
                    artist = $set.artist; title = $set.title; version = $map.version; creator = $set.creator
                    ranked_date = $set.ranked_date; last_updated = $set.last_updated; status = 'ranked'; mode = 'mania'
                })
            }
        }
        $cursor = $page.cursor_string
        Write-Host ("Catalogued {0:N0} mania beatmaps..." -f $rows.Count)
    } while ($cursor)
    $rows | Sort-Object beatmap_id | ForEach-Object { $_ | ConvertTo-Json -Compress } | Set-Content -LiteralPath $catalogPath -Encoding utf8
}

$catalogue = @(Get-Content -LiteralPath $catalogPath | Where-Object { $_.Trim() } | ForEach-Object { $_ | ConvertFrom-Json })
if ($catalogue.Count -eq 0) { throw 'The mania Ranked catalogue is empty.' }

# Use a CSV manifest as the portable load source. Its columns exactly correspond to
# the SQLite table, including per-file state; no existing std database is touched.
$manifest = foreach ($map in $catalogue) {
    $destination = Join-Path $beatmapPath ("{0}.osu" -f $map.beatmap_id)
    $status = if (Test-Path -LiteralPath $destination -PathType Leaf) { 'downloaded' } else { 'pending' }
    [pscustomobject]@{
        beatmap_id = $map.beatmap_id; beatmapset_id = $map.beatmapset_id; checksum = $map.checksum
        artist = $map.artist; title = $map.title; version = $map.version; creator = $map.creator
        ranked_date = $map.ranked_date; last_updated = $map.last_updated; status = 'ranked'; mode = 'mania'
        downloaded_path = if ($status -eq 'downloaded') { $destination } else { $null }
        downloaded_at = $null; download_status = $status; last_error = $null
        catalogued_at = (Get-Date).ToUniversalTime().ToString('o')
    }
}
$manifest | Export-Csv -LiteralPath $manifestPath -NoTypeInformation -Encoding utf8

$pending = @($manifest | Where-Object { $_.download_status -eq 'pending' })
Write-Host ("Catalogue: {0:N0}; pending downloads: {1:N0}." -f $manifest.Count, $pending.Count)

$failed = [System.Collections.Concurrent.ConcurrentBag[object]]::new()
$pending | ForEach-Object -Parallel {
    $map = $_
    $bag = $using:failed
    $headers = @{ Cookie = $using:cookieHeader; Accept = 'application/octet-stream' }
    $destination = Join-Path $using:beatmapPath ("{0}.osu" -f $map.beatmap_id)
    $partial = "$destination.part"
    $success = $false
    $lastError = $null
    for ($attempt = 1; $attempt -le $using:RetryCount -and -not $success; $attempt++) {
        try {
            $handler = [Net.Http.HttpClientHandler]::new()
            if ($using:Proxy) { $handler.Proxy = [Net.WebProxy]::new($using:Proxy); $handler.UseProxy = $true }
            $client = [Net.Http.HttpClient]::new($handler)
            $client.Timeout = [TimeSpan]::FromMinutes(10)
            $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, "https://osu.ppy.sh/osu/$($map.beatmap_id)")
            [void]$request.Headers.TryAddWithoutValidation('Cookie', $using:cookieHeader)
            $response = $client.SendAsync($request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            $response.EnsureSuccessStatusCode()
            $input = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $output = [IO.File]::Create($partial)
            $buffer = [byte[]]::new(65536); $total = 0L; $windowBytes = 0L
            $windowStart = [Diagnostics.Stopwatch]::GetTimestamp()
            try {
                while (($read = $input.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $output.Write($buffer, 0, $read); $total += $read; $windowBytes += $read
                    $elapsed = ([Diagnostics.Stopwatch]::GetTimestamp() - $windowStart) / [Diagnostics.Stopwatch]::Frequency
                    if ($elapsed -ge $using:LowSpeedWindowSeconds) {
                        $rate = $windowBytes / $elapsed
                        if ($rate -lt $using:MinimumBytesPerSecond) { throw "download speed ${rate} B/s stayed below $using:MinimumBytesPerSecond B/s" }
                        $windowBytes = 0L; $windowStart = [Diagnostics.Stopwatch]::GetTimestamp()
                    }
                }
            } finally { $output.Dispose(); $input.Dispose(); $response.Dispose(); $client.Dispose(); $handler.Dispose() }
            if ($total -eq 0) { throw 'Downloaded file is empty.' }
            Move-Item -LiteralPath $partial -Destination $destination -Force
            $success = $true
        }
        catch {
            $lastError = $_.Exception.Message
            Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
            if ($attempt -lt $using:RetryCount) { Start-Sleep -Seconds (2 * $attempt) }
        }
    }
    if (-not $success) {
        $bag.Add([pscustomobject]@{ beatmap_id = $map.beatmap_id; error = $lastError })
    }
} -ThrottleLimit $DownloadConcurrency

$failedById = @{}
foreach ($failure in $failed) { $failedById[[string]$failure.beatmap_id] = $failure.error }
foreach ($row in $manifest) {
    $destination = Join-Path $beatmapPath ("{0}.osu" -f $row.beatmap_id)
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        $row.download_status = 'downloaded'
        $row.downloaded_path = $destination
        $row.downloaded_at = (Get-Item -LiteralPath $destination).LastWriteTimeUtc.ToString('o')
        $row.last_error = $null
    }
    elseif ($failedById.ContainsKey([string]$row.beatmap_id)) {
        $row.download_status = 'failed'
        $row.last_error = $failedById[[string]$row.beatmap_id]
    }
}
$manifest | Export-Csv -LiteralPath $manifestPath -NoTypeInformation -Encoding utf8
if ($sqliteAvailable) { Import-ManifestIntoDatabase }

if ($failed.Count -gt 0) {
    $failed | Sort-Object beatmap_id | Export-Csv -LiteralPath $failurePath -NoTypeInformation -Encoding utf8
    throw ("{0} downloads failed; see $failurePath. Re-running resumes completed files." -f $failed.Count)
}

Write-Host ("Downloaded {0:N0} Ranked osu!mania .osu files to {1}." -f $manifest.Count, $beatmapPath)
Write-Host "SQLite catalogue: $databasePath"

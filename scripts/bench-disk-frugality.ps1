<#
.SYNOPSIS
    Measure Quiver's disk-resident memory-frugality wedge on Windows.

.DESCRIPTION
    One command, pure Rust — no Docker, no Python. Builds the `disk_recall`
    example, constructs the encrypted disk-resident DiskANN/Vamana + PQ index,
    serves it through mmap, and measures the **serving** resident set (RSS) at
    steady state — the number that actually backs Quiver's "serve a large
    dataset from a small RAM budget" claim.

    Why run this on Windows native instead of WSL2? Same physical box, but you
    can idle the machine and we bypass WSL2's memory-VM accounting, so the RSS
    reading is more trustworthy. NOTE: this is your dev hardware, not the
    documented reference hardware in docs/benchmarks/reference-hardware-runbook.md,
    so the output is labelled `dev-box · indicative`, never a published headline.
    This measures Quiver ONLY (the wedge). The multi-DB head-to-head vs
    Qdrant/FAISS/LanceDB is Linux/Docker-first and is NOT what this script does.

.PARAMETER Dataset
    siftsmall (default, ~5 MB, proves the mechanism), sift1m (~168 MB download,
    shows the wedge), or gist1m (~2.6 GB download, the 960-d stress test).

.PARAMETER TargetRecall
    Operating point to report (default 0.95). The closest l_search at or above
    it is chosen; if none reaches it, the best achieved is reported.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\bench-disk-frugality.ps1
    powershell -ExecutionPolicy Bypass -File scripts\bench-disk-frugality.ps1 -Dataset sift1m
#>
[CmdletBinding()]
param(
    [ValidateSet('siftsmall', 'sift1m', 'gist1m')]
    [string]$Dataset = 'siftsmall',
    [double]$TargetRecall = 0.95
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

function Need($cmd, $hint) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        throw "'$cmd' not found. $hint"
    }
}
Need cargo 'Install Rust from https://rustup.rs then re-run.'
Need tar   'tar ships with Windows 10 1803+. Update Windows or install it.'

# TEXMEX corpus (http://corpus-texmex.irisa.fr/). Pulled once, cached, gitignored.
$dirName = @{ siftsmall = 'siftsmall'; sift1m = 'sift'; gist1m = 'gist' }[$Dataset]
$url     = "ftp://ftp.irisa.fr/local/texmex/corpus/$dirName.tar.gz"
$dataDir = Join-Path $repo "bench\datasets\$dirName"
$base    = Join-Path $dataDir "${dirName}_base.fvecs"
$query   = Join-Path $dataDir "${dirName}_query.fvecs"
$gt      = Join-Path $dataDir "${dirName}_groundtruth.ivecs"

if (-not (Test-Path $base)) {
    Write-Host "Dataset '$Dataset' not present — downloading $url (one-time, cached)..." -ForegroundColor Cyan
    New-Item -ItemType Directory -Force -Path (Join-Path $repo 'bench\datasets') | Out-Null
    $archive = Join-Path $repo "bench\datasets\$dirName.tar.gz"
    (New-Object System.Net.WebClient).DownloadFile($url, $archive)   # WebClient handles ftp://
    Write-Host "Extracting..." -ForegroundColor Cyan
    tar -xzf $archive -C (Join-Path $repo 'bench\datasets')
}
foreach ($f in @($base, $query, $gt)) {
    if (-not (Test-Path $f)) { throw "Expected dataset file missing after extract: $f" }
}

Write-Host "Building disk_recall (release)..." -ForegroundColor Cyan
cargo build --release --example disk_recall -p quiverdb-index | Out-Host
$exe = Join-Path $repo 'target\release\examples\disk_recall.exe'
if (-not (Test-Path $exe)) { throw "Build produced no $exe" }

$out = Join-Path $repo "bench\results\windows-local"
New-Item -ItemType Directory -Force -Path $out | Out-Null
$index = Join-Path $out "$Dataset.qvx"

# --- build phase: capture the resident-codes vs full-precision arithmetic ---
Write-Host "`n=== BUILD: constructing encrypted disk index ===" -ForegroundColor Yellow
$buildLog = (& $exe build $base $index 2>&1 | Out-String); Write-Host $buildLog
$pqRamMb   = if ($buildLog -match 'RAM-resident codes:\s*([\d.]+)\s*MB')  { [double]$Matches[1] } else { $null }
$fullRamMb = if ($buildLog -match 'full-precision:\s*([\d.]+)\s*MB')      { [double]$Matches[1] } else { $null }
$onDiskMb  = if ($buildLog -match 'on-disk index:\s*([\d.]+)\s*MB')       { [double]$Matches[1] } else { $null }

# --- serve phase: sample the serving process RSS while it holds steady ---
Write-Host "`n=== SERVE: querying through mmap, sampling RSS ===" -ForegroundColor Yellow
$serveOutFile = Join-Path $out "$Dataset.serve.txt"
$env:QUIVER_DISK_HOLD_SECS = '8'   # hold at steady state so the sampler can read RSS
$proc = Start-Process -FilePath $exe -ArgumentList @('serve', $index, $query, $gt) `
    -PassThru -NoNewWindow -RedirectStandardOutput $serveOutFile
$peakRssMb = 0.0
while (-not $proc.HasExited) {
    try { $ws = (Get-Process -Id $proc.Id -ErrorAction Stop).WorkingSet64 / 1MB
          if ($ws -gt $peakRssMb) { $peakRssMb = $ws } } catch { }
    Start-Sleep -Milliseconds 200
}
Remove-Item Env:\QUIVER_DISK_HOLD_SECS
$serveOut = Get-Content $serveOutFile
$serveOut | Out-Host

# Parse the l_search / recall@10 / qps sweep table.
$rows = foreach ($line in $serveOut) {
    if ($line -match '^\s*(\d+)\s+([\d.]+)\s+(\d+)\s*$') {
        [pscustomobject]@{ l_search = [int]$Matches[1]; recall = [double]$Matches[2]; qps = [int]$Matches[3] }
    }
}
if (-not $rows) { throw "No serve results parsed — see $serveOutFile" }
$op = $rows | Where-Object { $_.recall -ge $TargetRecall } | Select-Object -First 1
if (-not $op) { $op = $rows | Sort-Object recall -Descending | Select-Object -First 1 }

# --- machine manifest (honesty: record the box the numbers came from) ---
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
$ramGb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$osCap = (Get-CimInstance Win32_OperatingSystem).Caption
$rustc = (rustc --version) 2>$null

$result = [ordered]@{
    dataset            = $Dataset
    operating_point    = "recall@10 >= $TargetRecall (or best)"
    recall_at_10       = $op.recall
    l_search           = $op.l_search
    qps_1t             = $op.qps
    serve_rss_mb       = [math]::Round($peakRssMb, 1)
    pq_codes_ram_mb    = $pqRamMb
    full_precision_ram_mb = $fullRamMb
    on_disk_index_mb   = $onDiskMb
    cpu = $cpu; ram_gb = $ramGb; os = $osCap; rustc = $rustc
    label = 'dev-box · indicative (not reference hardware)'
    generated_utc = (Get-Date).ToUniversalTime().ToString('u')
}
$jsonPath = Join-Path $out "$Dataset.frugality.json"
$result | ConvertTo-Json | Set-Content $jsonPath

# --- verdict ---
Write-Host "`n================ MEMORY-FRUGALITY VERDICT ================" -ForegroundColor Green
Write-Host ("Dataset            : {0}" -f $Dataset)
Write-Host ("Operating point    : recall@10 = {0:N4}  (l_search={1}, {2} QPS 1T)" -f $op.recall, $op.l_search, $op.qps)
Write-Host ("Disk-path SERVE RSS: {0:N1} MB   <-- the frugal serving footprint" -f $peakRssMb) -ForegroundColor Green
if ($fullRamMb) {
    Write-Host ("Full-precision floor: {0:N1} MB   (RAM just to HOLD the vectors in memory)" -f $fullRamMb)
    if ($peakRssMb -gt 0) {
        Write-Host ("Wedge              : {0:N1}x less RAM than the in-memory full-precision floor" -f ($fullRamMb / $peakRssMb)) -ForegroundColor Green
    }
}
Write-Host ("PQ codes resident  : {0:N1} MB (exact, host-independent)" -f $pqRamMb)
Write-Host ("On-disk index      : {0:N1} MB" -f $onDiskMb)
Write-Host ("Box                : {0} / {1} GB / {2}" -f $cpu, $ramGb, $osCap)
Write-Host "Label              : dev-box - indicative (NOT reference hardware; never a published headline)" -ForegroundColor DarkYellow
Write-Host ("Saved              : {0}" -f $jsonPath)
Write-Host "==========================================================" -ForegroundColor Green
if ($Dataset -eq 'siftsmall') {
    Write-Host "`nNote: siftsmall is 5 MB, so RSS here is mostly process baseline — the wedge" -ForegroundColor DarkGray
    Write-Host "only opens up at scale. Re-run with -Dataset sift1m (512 MB full-precision" -ForegroundColor DarkGray
    Write-Host "floor) or -Dataset gist1m (3.8 GB floor) to see it." -ForegroundColor DarkGray
}

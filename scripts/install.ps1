# Quiver — one-command installer for Windows (PowerShell 5.1+) (ADR-0039).
#
# Usage (run in PowerShell as your user — no admin required):
#   irm https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.ps1 | iex
#
# Environment overrides:
#   $env:QUIVER_VERSION      specific version to install (e.g. "0.17.0"); default: latest
#   $env:QUIVER_INSTALL_DIR  directory to install the binary to;
#                            default: $env:LOCALAPPDATA\quiver\bin
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repo       = 'achref-soua/quiver'
$InstallDir = if ($env:QUIVER_INSTALL_DIR) { $env:QUIVER_INSTALL_DIR } `
              else { Join-Path $env:LOCALAPPDATA 'quiver\bin' }

# ── helpers ──────────────────────────────────────────────────────────────────

function Write-Color {
    param([string]$Text, [ConsoleColor]$Color = 'White')
    $prev = $Host.UI.RawUI.ForegroundColor
    $Host.UI.RawUI.ForegroundColor = $Color
    Write-Host $Text
    $Host.UI.RawUI.ForegroundColor = $prev
}

function Write-Info { param($Msg) Write-Host "  $Msg" -ForegroundColor Cyan }
function Write-Ok   { param($Msg) Write-Host "  $Msg" -ForegroundColor Green }
function Write-Warn { param($Msg) Write-Host "  ! $Msg" -ForegroundColor Yellow }
function Fail       { param($Msg) Write-Host "`n  ERROR: $Msg" -ForegroundColor Red; exit 1 }

function Show-Logo {
    param([string]$Version = '')
    Write-Host ''
    Write-Color '    ██████╗ ██╗   ██╗██╗██╗   ██╗███████╗██████╗ ' DarkYellow
    Write-Color '   ██╔═══██╗██║   ██║██║██║   ██║██╔════╝██╔══██╗' DarkYellow
    Write-Color '   ██║   ██║██║   ██║██║╚██╗ ██╔╝█████╗  ██████╔╝' Yellow
    Write-Color '   ██║▄▄ ██║██║   ██║██║ ╚████╔╝ ██╔══╝  ██╔══██╗' Yellow
    Write-Color '   ╚██████╔╝╚██████╔╝██║  ╚██╔╝  ███████╗██║  ██║' DarkGreen
    Write-Color '    ╚══▀▀═╝  ╚═════╝ ╚═╝   ╚═╝   ╚══════╝╚═╝  ╚═╝' DarkGreen
    if ($Version) {
        $pad = ' ' * [Math]::Max(0, (48 - $Version.Length) / 2)
        Write-Color "${pad}security-first vector database  v${Version}" DarkCyan
    } else {
        Write-Color '        security-first vector database' DarkCyan
    }
    Write-Host ''
    Write-Color '  ┌──────────────────────────────────────────────┐' DarkGray
    Write-Color '  │  encrypted · memory-frugal · self-hostable   │' DarkGray
    Write-Color '  └──────────────────────────────────────────────┘' DarkGray
    Write-Host ''
}

# ── platform detection ────────────────────────────────────────────────────────

function Get-QuiverArch {
    # $env:PROCESSOR_ARCHITECTURE works on both PowerShell 5.1 (.NET Framework)
    # and PowerShell 7+ (.NET Core). Values: AMD64, ARM64, x86.
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { return 'x86_64' }
        'ARM64' { return 'aarch64' }
        default { Fail "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
    }
}

# ── checksum verification ─────────────────────────────────────────────────────

function Confirm-Sha256 {
    param([string]$FilePath, [string]$ChecksumFilePath)
    $checksumContent = (Get-Content -Raw $ChecksumFilePath).Trim()
    $expected = ($checksumContent -split '\s+')[0].ToLower()
    $actual   = (Get-FileHash -Algorithm SHA256 -Path $FilePath).Hash.ToLower()
    if ($actual -ne $expected) {
        Fail "SHA-256 mismatch.`n    expected: $expected`n    got:      $actual"
    }
}

# ── progress bar download ─────────────────────────────────────────────────────

function Get-FileWithProgress {
    param([string]$Uri, [string]$OutFile, [string]$Label)
    $ProgressPreference = 'SilentlyContinue'   # suppress PS default bar (slow on 5.1)
    Write-Host "  Downloading $Label" -NoNewline -ForegroundColor Cyan
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing
    } catch {
        Write-Host ''
        Fail "download failed: $_"
    }
    $sw.Stop()
    $size = [Math]::Round((Get-Item $OutFile).Length / 1MB, 1)
    Write-Host (" [{0:N1} MB in {1:N1}s] " -f $size, $sw.Elapsed.TotalSeconds) -ForegroundColor DarkGray
    Write-Host "  [" -NoNewline -ForegroundColor DarkGray
    Write-Host "##################################################" -NoNewline -ForegroundColor Green
    Write-Host "] 100%" -ForegroundColor DarkGray
}

# ── spinner helper ────────────────────────────────────────────────────────────

function Show-Step {
    param([string]$Icon, [string]$Msg, [ConsoleColor]$Color = 'Cyan')
    Write-Host "  $Icon " -NoNewline -ForegroundColor $Color
    Write-Host $Msg -ForegroundColor White
}

# ── main ──────────────────────────────────────────────────────────────────────

function Main {
    $Arch = Get-QuiverArch

    # ── version resolution ────────────────────────────────────────────────────
    $ProgressPreference = 'SilentlyContinue'
    Show-Step '⟳' 'Checking latest release...' Cyan

    $Version = ''
    if ($env:QUIVER_VERSION) {
        $Version = $env:QUIVER_VERSION.TrimStart('v')
    } else {
        $ApiUrl  = "https://api.github.com/repos/$Repo/releases/latest"
        $Headers = @{ 'User-Agent' = 'quiver-install-ps1'; 'Accept' = 'application/vnd.github+json' }
        try {
            $Release = Invoke-RestMethod -Uri $ApiUrl -Headers $Headers
            $Version = $Release.tag_name.TrimStart('v')
        } catch {
            Fail "could not reach GitHub API: $_"
        }
        if (-not $Version) { Fail 'could not determine latest version' }
    }

    # Show logo with resolved version
    Show-Logo -Version $Version

    $Asset       = "quiver-windows-$Arch.exe"
    $BaseUrl     = "https://github.com/$Repo/releases/download/v$Version/$Asset"
    $ChecksumUrl = "$BaseUrl.sha256"
    $TmpDir      = Join-Path ([System.IO.Path]::GetTempPath()) "quiver-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $TmpDir | Out-Null

    Show-Step '⬇' "Fetching v${Version} for windows/${Arch}..." Cyan

    try {
        $BinaryTmp   = Join-Path $TmpDir $Asset
        $ChecksumTmp = Join-Path $TmpDir "$Asset.sha256"

        Get-FileWithProgress -Uri $BaseUrl      -OutFile $BinaryTmp   -Label $Asset
        Get-FileWithProgress -Uri $ChecksumUrl  -OutFile $ChecksumTmp -Label "$Asset.sha256"

        Show-Step '🔒' 'Verifying SHA-256 checksum...' Yellow
        Confirm-Sha256 -FilePath $BinaryTmp -ChecksumFilePath $ChecksumTmp
        Show-Step '✔' 'Checksum verified.' Green

        if (-not (Test-Path $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir | Out-Null
        }
        $Dest = Join-Path $InstallDir 'quiver.exe'
        Copy-Item -Force $BinaryTmp $Dest

        Write-Host ''
        Write-Color '  ┌──────────────────────────────────────────────┐' DarkGray
        Write-Color ("  │  ✔  Quiver v{0,-37}│" -f "$Version installed!") Green
        Write-Color ("  │     {0,-45}│" -f $Dest) DarkGray
        Write-Color '  └──────────────────────────────────────────────┘' DarkGray
        Write-Host ''

        $UserPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
        if ($UserPath -notlike "*$InstallDir*") {
            Write-Warn "$InstallDir is not in your PATH. Add it:"
            Write-Host ''
            Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$InstallDir`", 'User')" -ForegroundColor DarkYellow
            Write-Host ''
            Write-Warn 'Restart your terminal after adding to PATH.'
            Write-Host ''
        }

        Write-Host '  Next steps:' -ForegroundColor White
        Write-Host '    quiver serve             ' -NoNewline -ForegroundColor DarkYellow
        Write-Host '# start the server (gRPC + REST on :6333)' -ForegroundColor DarkGray
        Write-Host '    quiver tui               ' -NoNewline -ForegroundColor DarkYellow
        Write-Host '# open the retro cockpit' -ForegroundColor DarkGray
        Write-Host '    quiver update            ' -NoNewline -ForegroundColor DarkYellow
        Write-Host '# self-update to the latest release' -ForegroundColor DarkGray
        Write-Host '    quiver --help            ' -NoNewline -ForegroundColor DarkYellow
        Write-Host '# all commands' -ForegroundColor DarkGray
        Write-Host ''

    } finally {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }
}

Main

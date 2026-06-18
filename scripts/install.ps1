# Quiver — one-command installer for Windows (PowerShell 5.1+) (ADR-0039).
#
# Usage (run in PowerShell as your user — no admin required):
#   irm https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.ps1 | iex
#
# Environment overrides:
#   $env:QUIVER_VERSION      specific version to install (e.g. "0.17.0"); default: latest
#   $env:QUIVER_INSTALL_DIR  directory to install the binary to;
#                            default: $env:LOCALAPPDATA\quiver\bin
#
# The script:
#   1. Detects architecture.
#   2. Resolves the target version via the GitHub Releases API (or $env:QUIVER_VERSION).
#   3. Downloads the binary and its SHA-256 checksum.
#   4. Verifies the checksum before writing anything to the install path.
#   5. Installs and prints a PATH hint if needed.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repo        = 'achref-soua/quiver'
$InstallDir  = if ($env:QUIVER_INSTALL_DIR) { $env:QUIVER_INSTALL_DIR } `
               else { Join-Path $env:LOCALAPPDATA 'quiver\bin' }

# ── helpers ──────────────────────────────────────────────────────────────────

function Write-Info  { param($Msg) Write-Host "[quiver] $Msg" -ForegroundColor Cyan }
function Write-Ok    { param($Msg) Write-Host "[quiver] $Msg" -ForegroundColor Green }
function Write-Warn  { param($Msg) Write-Host "[quiver] warning: $Msg" -ForegroundColor Yellow }
function Fail        { param($Msg) Write-Error "[quiver] error: $Msg"; exit 1 }

# ── platform detection ────────────────────────────────────────────────────────

function Get-QuiverArch {
    $pa = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
    switch ($pa) {
        'X64'   { return 'x86_64' }
        'Arm64' { return 'aarch64' }
        default { Fail "unsupported architecture: $pa" }
    }
}

# ── checksum verification ─────────────────────────────────────────────────────

function Confirm-Sha256 {
    param(
        [string]$FilePath,
        [string]$ChecksumFilePath
    )
    $checksumContent = (Get-Content -Raw $ChecksumFilePath).Trim()
    # The checksum file may be "hash  filename" or just "hash"
    $expected = ($checksumContent -split '\s+')[0].ToLower()
    $actual   = (Get-FileHash -Algorithm SHA256 -Path $FilePath).Hash.ToLower()
    if ($actual -ne $expected) {
        Fail "SHA-256 checksum mismatch.`n  Expected: $expected`n  Got:      $actual`nAborting install."
    }
}

# ── main ──────────────────────────────────────────────────────────────────────

function Main {
    $Arch  = Get-QuiverArch
    $Asset = "quiver-windows-$Arch.exe"   # Windows target name

    # Resolve version
    if ($env:QUIVER_VERSION) {
        $Version = $env:QUIVER_VERSION.TrimStart('v')
        Write-Info "Installing quiver v$Version (pinned)"
    } else {
        Write-Info "Resolving latest release..."
        $ApiUrl  = "https://api.github.com/repos/$Repo/releases/latest"
        $Headers = @{ 'User-Agent' = 'quiver-install-ps1'; 'Accept' = 'application/vnd.github+json' }
        $Release = Invoke-RestMethod -Uri $ApiUrl -Headers $Headers
        $Version = $Release.tag_name.TrimStart('v')
        if (-not $Version) { Fail "could not determine latest version from GitHub API" }
        Write-Info "Latest version: v$Version"
    }

    $BaseUrl     = "https://github.com/$Repo/releases/download/v$Version/$Asset"
    $ChecksumUrl = "$BaseUrl.sha256"
    $TmpDir      = [System.IO.Path]::GetTempPath() | Join-Path -ChildPath "quiver-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $TmpDir | Out-Null

    try {
        $BinaryTmp   = Join-Path $TmpDir $Asset
        $ChecksumTmp = Join-Path $TmpDir "$Asset.sha256"

        Write-Info "Downloading $Asset..."
        Invoke-WebRequest -Uri $BaseUrl      -OutFile $BinaryTmp   -UseBasicParsing
        Write-Info "Downloading checksum..."
        Invoke-WebRequest -Uri $ChecksumUrl  -OutFile $ChecksumTmp -UseBasicParsing

        Write-Info "Verifying SHA-256 checksum..."
        Confirm-Sha256 -FilePath $BinaryTmp -ChecksumFilePath $ChecksumTmp
        Write-Ok "Checksum verified."

        # Install
        if (-not (Test-Path $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir | Out-Null
        }
        $Dest = Join-Path $InstallDir 'quiver.exe'
        Copy-Item -Force $BinaryTmp $Dest

        Write-Ok "Quiver v$Version installed to $Dest"

        # PATH hint
        $UserPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
        if ($UserPath -notlike "*$InstallDir*") {
            Write-Warn "$InstallDir is not in your PATH."
            Write-Warn "Add it with:"
            Write-Warn "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$InstallDir`", 'User')"
        }

        Write-Ok "Run 'quiver --version' to confirm."
        Write-Ok "Run 'quiver serve' to start the server, or 'quiver --help' for all commands."
    } finally {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }
}

Main

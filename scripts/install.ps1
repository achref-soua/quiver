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

# Ensure Unicode block characters render correctly on Windows.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$Repo       = 'achref-soua/quiver'
$InstallDir = if ($env:QUIVER_INSTALL_DIR) { $env:QUIVER_INSTALL_DIR } `
              else { Join-Path $env:LOCALAPPDATA 'quiver\bin' }

# ── ANSI true-color helpers ───────────────────────────────────────────────────
# [char]27 for ESC works in both PowerShell 5.1 (.NET Framework) and 7+.
# VT/ANSI processing is enabled by default on Windows 10 build 14393+.

$E  = [char]27
$B  = "${E}[38;2;205;127;50m"  # bronze    #CD7F32  theme CHROME
$V  = "${E}[38;2;63;182;168m"  # verdigris #3FB6A8  theme ACCENT
$G  = "${E}[38;2;143;179;57m"  # green     #8FB339  theme OK
$GR = "${E}[38;2;90;90;90m"    # dark gray
$W  = "${E}[38;2;230;230;230m" # parchment/white
$Y  = "${E}[38;2;215;200;0m"   # yellow    (warnings)
$RE = "${E}[38;2;210;85;47m"   # red       theme ALERT
$R  = "${E}[0m"                 # reset

function Write-Ok   { param($Msg) Write-Host "  ${G}✔${R}  $Msg" }
function Write-Warn { param($Msg) Write-Host "  ${Y}!${R}  $Msg" }
function Fail       { param($Msg) Write-Host "`n  ${RE}ERROR: $Msg${R}"; exit 1 }

function Show-Logo {
    param([string]$Version = '')
    Write-Host ''
    Write-Host "${B}    ██████╗ ██╗   ██╗██╗${R}${V}██╗   ██╗${R}${B}███████╗██████╗ ${R}"
    Write-Host "${B}   ██╔═══██╗██║   ██║██║${R}${V}██║   ██║${R}${B}██╔════╝██╔══██╗${R}"
    Write-Host "${B}   ██║   ██║██║   ██║██║${R}${V}╚██╗ ██╔╝${R}${B}█████╗  ██████╔╝${R}"
    Write-Host "${B}   ██║▄▄ ██║██║   ██║██║${R}${V} ╚████╔╝ ${R}${B}██╔══╝  ██╔══██╗${R}"
    Write-Host "${B}   ╚██████╔╝╚██████╔╝██║${R}${V}  ╚██╔╝  ${R}${B}███████╗██║  ██║${R}"
    Write-Host "${B}    ╚══▀▀═╝  ╚═════╝ ╚═╝${R}${V}   ╚═╝   ${R}${B}╚══════╝╚═╝  ╚═╝${R}"
    if ($Version) {
        Write-Host "${V}        security-first vector database  v${Version}${R}"
    } else {
        Write-Host "${V}        security-first vector database${R}"
    }
    Write-Host ''
    Write-Host "${GR}  ┌──────────────────────────────────────────────┐${R}"
    Write-Host "${GR}  │  encrypted · memory-frugal · self-hostable   │${R}"
    Write-Host "${GR}  └──────────────────────────────────────────────┘${R}"
    Write-Host ''
}

function Show-Step {
    param([string]$Icon, [string]$Msg)
    Write-Host "  ${V}$Icon${R}  ${W}$Msg${R}"
}

# ── platform detection ────────────────────────────────────────────────────────

function Get-QuiverArch {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { return 'x86_64' }
        'ARM64' { return 'aarch64' }
        default { Fail "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
    }
}

# ── checksum verification ─────────────────────────────────────────────────────

function Confirm-Sha256 {
    param([string]$FilePath, [string]$ChecksumFilePath)
    $content  = (Get-Content -Raw $ChecksumFilePath).Trim()
    $expected = ($content -split '\s+')[0].ToLower()
    $actual   = (Get-FileHash -Algorithm SHA256 -Path $FilePath).Hash.ToLower()
    if ($actual -ne $expected) {
        Fail "SHA-256 mismatch.`n    expected: $expected`n    got:      $actual"
    }
}

# ── progress bar download ─────────────────────────────────────────────────────

function Get-FileWithProgress {
    param([string]$Uri, [string]$OutFile, [string]$Label)
    $ProgressPreference = 'SilentlyContinue'
    Write-Host "  ${V}⬇${R}  ${W}$Label${R}" -NoNewline
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing
    } catch {
        Write-Host ''
        Fail "download failed: $_"
    }
    $sw.Stop()
    $size = [Math]::Round((Get-Item $OutFile).Length / 1MB, 1)
    Write-Host ("  ${GR}[{0:N1} MB in {1:N1}s]${R}" -f $size, $sw.Elapsed.TotalSeconds)
    Write-Host "  ${GR}[${G}##################################################${GR}] 100%${R}"
}

# ── shortcut creation ─────────────────────────────────────────────────────────
# Creates a Desktop icon and a Start Menu entry.  The icon is pulled from the
# embedded resource in quiver.exe so Finder/Explorer and the taskbar show the
# arrowhead logo.  Both shortcuts launch `quiver demo` inside cmd /k so the
# window stays open while the TUI cockpit runs.

function New-QuiverShortcut {
    param([string]$LinkPath, [string]$ExePath)
    $shell  = New-Object -ComObject WScript.Shell
    $sc     = $shell.CreateShortcut($LinkPath)
    $sc.TargetPath       = $env:ComSpec
    $sc.Arguments        = "/k `"$ExePath`" demo"
    $sc.WorkingDirectory = $env:USERPROFILE
    $sc.Description      = 'Security-first, memory-frugal vector database'
    $sc.IconLocation     = "$ExePath,0"   # resource index 0 = embedded quiver icon
    $sc.WindowStyle      = 1              # 1 = normal window
    $sc.Save()
}

# ── main ──────────────────────────────────────────────────────────────────────

function Main {
    $Arch = Get-QuiverArch

    $ProgressPreference = 'SilentlyContinue'
    Show-Step '⟳' 'Checking latest release...'

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

    Show-Logo -Version $Version

    $Asset       = "quiver-windows-$Arch.exe"
    $BaseUrl     = "https://github.com/$Repo/releases/download/v$Version/$Asset"
    $ChecksumUrl = "$BaseUrl.sha256"
    $TmpDir      = Join-Path ([System.IO.Path]::GetTempPath()) "quiver-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $TmpDir | Out-Null

    Show-Step '⬇' "Fetching v${Version} for windows/${Arch}..."

    try {
        $BinaryTmp   = Join-Path $TmpDir $Asset
        $ChecksumTmp = Join-Path $TmpDir "$Asset.sha256"

        Get-FileWithProgress -Uri $BaseUrl     -OutFile $BinaryTmp   -Label $Asset
        Get-FileWithProgress -Uri $ChecksumUrl -OutFile $ChecksumTmp -Label "$Asset.sha256"

        Show-Step '🔒' 'Verifying SHA-256 checksum...'
        Confirm-Sha256 -FilePath $BinaryTmp -ChecksumFilePath $ChecksumTmp
        Write-Ok 'Checksum verified.'

        if (-not (Test-Path $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir | Out-Null
        }
        $Dest = Join-Path $InstallDir 'quiver.exe'
        Copy-Item -Force $BinaryTmp $Dest

        # ── auto-add to PATH (no manual steps) ───────────────────────────────
        $UserPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
        if ($UserPath -notlike "*$InstallDir*") {
            [Environment]::SetEnvironmentVariable('PATH', "$UserPath;$InstallDir", 'User')
            $env:PATH += ";$InstallDir"
            Write-Ok "Added $InstallDir to your PATH."
        } else {
            Write-Ok "$InstallDir is already in your PATH."
        }

        # ── Desktop + Start Menu shortcuts ────────────────────────────────────
        Show-Step '🖼' 'Creating Desktop and Start Menu shortcuts...'
        try {
            $Desktop   = [System.Environment]::GetFolderPath('Desktop')
            $StartMenu = [System.IO.Path]::Combine(
                [System.Environment]::GetFolderPath('StartMenu'), 'Programs')
            New-QuiverShortcut -LinkPath (Join-Path $Desktop   'Quiver.lnk') -ExePath $Dest
            New-QuiverShortcut -LinkPath (Join-Path $StartMenu 'Quiver.lnk') -ExePath $Dest
            Write-Ok "Desktop icon created — double-click to launch the cockpit."
            Write-Host "    ${GR}Tip: right-click the Desktop icon → ${W}Pin to taskbar${GR} to dock it.${R}"
        } catch {
            Write-Warn "Could not create shortcuts: $_"
        }

        # ── adaptive success box ─────────────────────────────────────────────
        $l1  = "  ✔  Quiver v$Version installed!"
        $l2  = "     $Dest"
        $w   = [Math]::Max($l1.Length, $l2.Length)
        $bar = '─' * ($w + 2)
        Write-Host ''
        Write-Host "${GR}  ┌${bar}┐${R}"
        Write-Host "  ${G}│ $($l1.PadRight($w)) │${R}"
        Write-Host "  ${GR}│ $($l2.PadRight($w)) │${R}"
        Write-Host "${GR}  └${bar}┘${R}"
        Write-Host ''

        Write-Host "  ${W}Or from the terminal:${R}"
        Write-Host "    ${B}quiver demo  ${R}  ${GR}# seed vectors + open cockpit${R}"
        Write-Host "    ${B}quiver serve ${R}  ${GR}# start the server (gRPC + REST on :6333)${R}"
        Write-Host "    ${B}quiver update${R}  ${GR}# self-update to the latest release${R}"
        Write-Host "    ${B}quiver --help${R}  ${GR}# all commands${R}"
        Write-Host ''

    } finally {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }
}

Main

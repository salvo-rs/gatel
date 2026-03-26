#Requires -Version 5.1
<#
.SYNOPSIS
    Gatel installer for Windows.

.DESCRIPTION
    Downloads and installs gatel, a high-performance reverse proxy and web server.
    Can install from prebuilt binaries or build from source.

.PARAMETER Prefix
    Installation directory (default: $env:LOCALAPPDATA\gatel).

.PARAMETER FromSource
    Build from source instead of downloading a prebuilt binary.

.PARAMETER Version
    Install a specific version (default: latest).

.PARAMETER AddToPath
    Add the install directory to the user PATH (default: true).

.EXAMPLE
    # One-liner install
    irm https://raw.githubusercontent.com/salvo-rs/gatel/main/install.ps1 | iex

    # Install with options
    .\install.ps1 -Prefix "C:\gatel" -FromSource
#>

param(
    [string]$Prefix = "$env:LOCALAPPDATA\gatel",
    [switch]$FromSource,
    [string]$Version = "latest",
    [bool]$AddToPath = $true
)

$ErrorActionPreference = "Stop"
$Repo = "salvo-rs/gatel"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Write-Info  { Write-Host "==> $args" -ForegroundColor Cyan }
function Write-Warn  { Write-Host "WARN: $args" -ForegroundColor Yellow }

function Test-Command([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-Arch {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64"   { return "x86_64" }
        "Arm64" { return "aarch64" }
        default { throw "Unsupported architecture: $arch" }
    }
}

# ---------------------------------------------------------------------------
# Install from source
# ---------------------------------------------------------------------------

function Install-FromSource {
    Write-Info "Installing gatel from source"

    if (-not (Test-Command "cargo")) {
        throw "Rust toolchain not found. Install from https://rustup.rs"
    }
    if (-not (Test-Command "git")) {
        throw "git not found. Install from https://git-scm.com"
    }

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "gatel-build-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        Write-Info "Cloning repository..."
        if ($Version -eq "latest") {
            git clone --depth 1 "https://github.com/$Repo.git" "$tmpDir\gatel"
        } else {
            git clone --depth 1 --branch $Version "https://github.com/$Repo.git" "$tmpDir\gatel"
        }

        if ($LASTEXITCODE -ne 0) { throw "git clone failed" }

        Push-Location "$tmpDir\gatel"
        Write-Info "Building release binaries..."
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        Pop-Location

        Install-Binaries "$tmpDir\gatel\target\release"
        Install-Extras
        Write-Info "Done! Run 'gatel --help' to get started."
    } finally {
        Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# Install from prebuilt binary
# ---------------------------------------------------------------------------

function Install-FromBinary {
    $arch = Get-Arch
    Write-Info "Detected: windows/$arch"

    $tag = $Version
    if ($Version -eq "latest") {
        try {
            $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -ErrorAction Stop
            $tag = $release.tag_name
        } catch {
            Write-Warn "No prebuilt release found. Falling back to source build."
            Install-FromSource
            return
        }
    }

    $assetName = "gatel-$tag-$arch-windows.zip"
    $downloadUrl = "https://github.com/$Repo/releases/download/$tag/$assetName"

    Write-Info "Downloading gatel $tag for windows/$arch..."

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "gatel-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        $zipPath = Join-Path $tmpDir "gatel.zip"
        try {
            Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -ErrorAction Stop
        } catch {
            Write-Warn "Binary download failed. Falling back to source build."
            Install-FromSource
            return
        }

        Write-Info "Extracting..."
        Expand-Archive -Path $zipPath -DestinationPath $tmpDir -Force

        Install-Binaries $tmpDir
        Install-Extras
        Write-Info "Installed gatel $tag"
        Write-Info "Run 'gatel --help' to get started."
    } finally {
        Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# Common installation steps
# ---------------------------------------------------------------------------

function Install-Binaries([string]$SrcDir) {
    $binDir = Join-Path $Prefix "bin"
    Write-Info "Installing binaries to $binDir"
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null

    foreach ($bin in @("gatel.exe", "gatel-passwd.exe", "gatel-precompress.exe")) {
        $src = Join-Path $SrcDir $bin
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $binDir $bin) -Force
        }
    }
}

function Install-Extras {
    # Create config directory
    $configDir = Join-Path $Prefix "etc"
    New-Item -ItemType Directory -Path $configDir -Force | Out-Null

    # Write default config if none exists
    $configFile = Join-Path $configDir "gatel.kdl"
    if (-not (Test-Path $configFile)) {
        @"
global {
    log level="info"
    http ":80"
}

site "*" {
    route "/*" {
        respond "Hello from gatel!" status=200
    }
}
"@ | Set-Content -Path $configFile -Encoding UTF8
        Write-Info "Default config written to $configFile"
    }

    # Add to PATH
    if ($AddToPath) {
        Add-ToUserPath (Join-Path $Prefix "bin")
    }

    # Install Windows Service helper
    Install-WindowsServiceHelper
}

function Add-ToUserPath([string]$Dir) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -split ";" | Where-Object { $_ -eq $Dir }) {
        return
    }
    Write-Info "Adding $Dir to user PATH"
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$Dir", "User")
    $env:Path = "$env:Path;$Dir"
}

function Install-WindowsServiceHelper {
    $binPath = Join-Path $Prefix "bin\gatel.exe"
    $configPath = Join-Path $Prefix "etc\gatel.kdl"

    Write-Info "To register as a Windows service (run as Administrator):"
    Write-Info "  & `"$binPath`" service install --config `"$configPath`""
    Write-Info "  sc start gatel"
    Write-Info ""
    Write-Info "To uninstall the service:"
    Write-Info "  & `"$binPath`" service uninstall"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if ($FromSource) {
    Install-FromSource
} else {
    Install-FromBinary
}

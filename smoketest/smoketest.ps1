#Requires -Version 5.1
<#
.SYNOPSIS
    Smoke test for gatel on Windows.

.PARAMETER Binary
    Path to the gatel binary (default: .\target\release\gatel.exe).
#>
param(
    [string]$Binary = ".\target\release\gatel.exe"
)

$ErrorActionPreference = "Stop"
$Port = 19876
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "gatel-smoke-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

function Write-Test { param($Name) Write-Host "[SMOKE] $Name" -ForegroundColor Cyan }
function Write-Pass { param($Name) Write-Host "[PASS]  $Name" -ForegroundColor Green }
function Write-Fail { param($Name) Write-Host "[FAIL]  $Name" -ForegroundColor Red; exit 1 }

try {
    if (-not (Test-Path $Binary)) {
        Write-Fail "Binary not found: $Binary"
    }

    # Test 1: --help
    Write-Test "--help"
    & $Binary --help 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Fail "--help" }
    Write-Pass "--help"

    # Test 2: --version
    Write-Test "--version"
    & $Binary --version 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Fail "--version" }
    Write-Pass "--version"

    # Test 3: validate config
    Write-Test "validate config"
    $configPath = Join-Path $TmpDir "test.kdl"
    @"
global {
    http ":$Port"
}
site "*" {
    route "/*" {
        respond "smoke-ok" status=200
    }
}
"@ | Set-Content $configPath -Encoding UTF8
    & $Binary validate --config $configPath 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Fail "validate" }
    Write-Pass "validate config"

    # Test 4: validate rejects bad config
    Write-Test "rejects bad config"
    $badPath = Join-Path $TmpDir "bad.kdl"
    "not valid kdl {{{{" | Set-Content $badPath -Encoding UTF8
    & $Binary validate --config $badPath 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) { Write-Fail "accepted bad config" }
    Write-Pass "rejects bad config"

    # Test 5: start, serve, stop
    Write-Test "start, serve, stop"
    $proc = Start-Process -FilePath $Binary -ArgumentList "run","--config",$configPath `
        -PassThru -NoNewWindow -RedirectStandardOutput "$TmpDir\out.log" `
        -RedirectStandardError "$TmpDir\err.log"

    Start-Sleep -Seconds 3

    try {
        $response = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/" -UseBasicParsing -TimeoutSec 5
        if ($response.Content -ne "smoke-ok") {
            Write-Fail "unexpected response: $($response.Content)"
        }
    } catch {
        Write-Fail "HTTP request failed: $_"
    }

    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Write-Pass "start, serve, stop"

    Write-Host ""
    Write-Host "[SMOKE] All smoke tests passed." -ForegroundColor Cyan

} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}

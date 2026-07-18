[CmdletBinding()]
param(
    [switch]$WindowsGnu
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if ($IsWindows -or $env:OS -eq "Windows_NT") {
        cargo run --target x86_64-pc-windows-gnu -p xtask -- quality
    } else {
        cargo run -p xtask -- quality
    }
    if ($LASTEXITCODE -ne 0) { throw "HD quality gate failed with $LASTEXITCODE" }
    if ($WindowsGnu) {
        & (Join-Path $root "build.bat")
        if ($LASTEXITCODE -ne 0) { throw "MinGW build failed with $LASTEXITCODE" }
    }
} finally {
    Pop-Location
}

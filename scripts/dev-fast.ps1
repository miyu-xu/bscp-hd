[CmdletBinding()]
param(
    [string[]]$Package = @("hd-runtime"),
    [switch]$Build,
    [switch]$Release
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$target = if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    "x86_64-pc-windows-gnu"
} else {
    $null
}

function Invoke-CargoStep {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string[]]$Arguments
    )

    $watch = [Diagnostics.Stopwatch]::StartNew()
    [Console]::WriteLine("[$Name] cargo $($Arguments -join ' ')")
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
    $watch.Stop()
    [pscustomobject][ordered]@{
        step = $Name
        duration_ms = [long]$watch.ElapsedMilliseconds
    }
}

Push-Location $root
try {
    $packageArguments = [Collections.Generic.List[string]]::new()
    foreach ($name in $Package) {
        if ($name -notmatch '^[a-zA-Z0-9_-]+$') {
            throw "Invalid Cargo package name: $name"
        }
        $packageArguments.Add("-p")
        $packageArguments.Add($name)
    }

    $records = [Collections.Generic.List[object]]::new()
    $formatArguments = @("fmt") + @($packageArguments) + @("--", "--check")
    $records.Add((Invoke-CargoStep -Name "fmt-check" -Arguments $formatArguments))

    $checkArguments = @("check") + @($packageArguments)
    if ($null -ne $target) {
        $checkArguments += @("--target", $target)
    }
    $records.Add((Invoke-CargoStep -Name "cargo-check" -Arguments $checkArguments))

    if ($Build -or $Release) {
        $buildArguments = @("build") + @($packageArguments)
        if ($Release) {
            $buildArguments += "--release"
        }
        if ($null -ne $target) {
            $buildArguments += @("--target", $target)
        }
        $records.Add((Invoke-CargoStep -Name "cargo-build" -Arguments $buildArguments))
    }

    [pscustomobject][ordered]@{
        mode = "fast-development"
        packages = $Package
        target = $target
        capability_probe = "skipped"
        bundle_validation = "skipped"
        steps = $records
    } | ConvertTo-Json -Depth 5
} finally {
    Pop-Location
}

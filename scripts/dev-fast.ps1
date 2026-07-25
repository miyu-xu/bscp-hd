[CmdletBinding()]
param(
    [string[]]$Package = @("hd-runtime"),
    [switch]$Build,
    [switch]$Release,
    [switch]$SyncDist
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

function Invoke-WebStep {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    [Console]::WriteLine("[web-build] npm run build")
    Push-Location (Join-Path $root "web")
    try {
        & npm.cmd run build
        if ($LASTEXITCODE -ne 0) {
            throw "web-build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
    $watch.Stop()
    [pscustomobject][ordered]@{
        step = "web-build"
        duration_ms = [long]$watch.ElapsedMilliseconds
    }
}

function Sync-WindowsDist {
    param(
        [Parameter(Mandatory)][string[]]$Packages,
        [Parameter(Mandatory)][string]$Profile
    )

    if ($null -eq $target) {
        throw "-SyncDist is supported only for Windows builds"
    }
    if (-not ($Build -or $Release)) {
        throw "-SyncDist requires -Build or -Release"
    }

    $binaryByPackage = @{
        "hd-ui" = "hd.exe"
        "hd-worker" = "hd-worker.exe"
        "hd-host" = "hd-host.exe"
        "hdctl" = "hdctl.exe"
        "hd-adb-bridge" = "hd-adb-bridge.exe"
        "hd-device-sim" = "hd-device-sim.exe"
        "hd-rootcanal-adapter" = "hd-rootcanal-adapter.exe"
        "hd-casimir-adapter" = "hd-casimir-adapter.exe"
        "hd-uwb-adapter" = "hd-uwb-adapter.exe"
        "hd-modem-adapter" = "hd-modem-adapter.exe"
        "hd-sensor-injector" = "hd-sensor-injector.exe"
    }
    $destination = Join-Path (Split-Path -Parent $root) "out\dist\windows\bin"
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    $copied = [Collections.Generic.List[string]]::new()
    foreach ($package in $Packages) {
        if (-not $binaryByPackage.ContainsKey($package)) {
            continue
        }
        $name = $binaryByPackage[$package]
        $source = Join-Path $root "target\$target\$Profile\$name"
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Built binary was not produced: $source"
        }
        $targetPath = Join-Path $destination $name
        $temporary = "$targetPath.new"
        Copy-Item -LiteralPath $source -Destination $temporary -Force
        Move-Item -LiteralPath $temporary -Destination $targetPath -Force
        $copied.Add($targetPath)
    }
    return $copied
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
    if ($Package -contains "hd-ui") {
        $webRoot = Join-Path $root "web"
        $distIndex = Join-Path $webRoot "dist\index.html"
        $sourceFiles = @(
            Get-ChildItem (Join-Path $webRoot "src") -Recurse -File
            Get-Item (Join-Path $webRoot "index.html")
            Get-Item (Join-Path $webRoot "package-lock.json")
            Get-Item (Join-Path $webRoot "vite.config.ts")
        )
        $newestSource = ($sourceFiles | Measure-Object LastWriteTimeUtc -Maximum).Maximum
        if (-not (Test-Path $distIndex) -or (Get-Item $distIndex).LastWriteTimeUtc -lt $newestSource) {
            $records.Add((Invoke-WebStep))
        }
    }
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

    $synced = @()
    if ($SyncDist) {
        $profile = if ($Release) { "release" } else { "debug" }
        $synced = @(Sync-WindowsDist -Packages $Package -Profile $profile)
    }

    [pscustomobject][ordered]@{
        mode = "fast-development"
        packages = $Package
        target = $target
        capability_probe = "skipped"
        bundle_validation = "skipped"
        synced_dist = $synced
        steps = $records
    } | ConvertTo-Json -Depth 5
} finally {
    Pop-Location
}

param(
    [Parameter(Mandatory = $true)][Guid]$InstanceId,
    [string]$DataRoot = "D:\hd-v2-data",
    [string]$Hdctl = "",
    [string]$Adb = "",
    [string]$Output = "out\windows-display-matrix"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$Hdctl = if ($Hdctl) { $Hdctl } else { Join-Path $repoRoot "out\dist\windows\bin\hdctl.exe" }
if (-not $Adb) {
    $sdkRoot = if ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT } else { $env:ANDROID_HOME }
    $Adb = if ($sdkRoot) {
        Join-Path $sdkRoot "platform-tools\adb.exe"
    } else {
        $adbCommand = Get-Command adb.exe -ErrorAction SilentlyContinue
        if ($adbCommand) { $adbCommand.Source } else { "adb.exe" }
    }
}
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $repoRoot $Output }
$records = [Collections.Generic.List[object]]::new()
$failure = $null

function Invoke-Hdctl {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & $script:Hdctl --data-root $script:DataRoot --no-start-host @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "hdctl $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join [Environment]::NewLine)
}

function Get-Instance {
    return (Invoke-Hdctl show $script:InstanceId.ToString() | ConvertFrom-Json)
}

function Stop-IfRunning {
    $instance = Get-Instance
    if ($instance.status.desired -eq "running") {
        Invoke-Hdctl stop $script:InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
    }
}

function Invoke-AdbShell {
    param([string]$Serial, [Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & $script:Adb -s $Serial shell @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "adb -s $Serial shell $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return (($output -join "`n").Trim())
}

if (-not (Test-Path -LiteralPath $Hdctl -PathType Leaf)) { throw "hdctl is missing: $Hdctl" }
if (-not (Test-Path -LiteralPath $Adb -PathType Leaf)) { throw "adb is missing: $Adb" }
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null

$matrix = @(
    [ordered]@{ name = "portrait-30"; width = 720; height = 1280; dpi = 240; refresh_rate_hz = 30; orientation = "portrait"; rotation = 0; vsync = "on" },
    [ordered]@{ name = "landscape-60"; width = 720; height = 1280; dpi = 320; refresh_rate_hz = 60; orientation = "landscape"; rotation = 1; vsync = "off" },
    [ordered]@{ name = "reverse-portrait-90"; width = 1080; height = 1920; dpi = 420; refresh_rate_hz = 90; orientation = "reverse_portrait"; rotation = 2; vsync = "on" },
    [ordered]@{ name = "reverse-landscape-120"; width = 1080; height = 1920; dpi = 480; refresh_rate_hz = 120; orientation = "reverse_landscape"; rotation = 3; vsync = "off" }
)

$initial = Get-Instance
$initialSpecJson = $initial.spec | ConvertTo-Json -Depth 20
$guestDigest = $initial.spec.artifacts.guest_bundle_digest
$hostDigest = $initial.spec.artifacts.host_bundle_digest

try {
    Stop-IfRunning
    foreach ($configuration in $matrix) {
        $spec = $initialSpecJson | ConvertFrom-Json
        $spec.display.width = $configuration.width
        $spec.display.height = $configuration.height
        $spec.display.dpi = $configuration.dpi
        $spec.display.refresh_rate_hz = $configuration.refresh_rate_hz
        $spec.display.orientation = $configuration.orientation
        $spec.display.vsync = $configuration.vsync
        $specPath = Join-Path $outputRoot "$($configuration.name)-spec.json"
        [IO.File]::WriteAllText($specPath, ($spec | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
        Invoke-Hdctl update $InstanceId.ToString() $specPath | Out-Null
        Invoke-Hdctl start $InstanceId.ToString() | Out-Null

        $instance = Get-Instance
        if ($instance.status.observed -ne "ready" -or [string]::IsNullOrWhiteSpace($instance.adb_serial)) {
            throw "$($configuration.name) did not reach strict Ready"
        }
        foreach ($field in @("width", "height", "dpi", "refresh_rate_hz", "orientation", "vsync")) {
            if ($instance.spec.display.$field -ne $configuration.$field) {
                throw "$($configuration.name) display field $field did not persist"
            }
        }
        $rotation = Invoke-AdbShell $instance.adb_serial settings get system user_rotation
        if ($rotation -ne $configuration.rotation.ToString()) {
            throw "$($configuration.name) Android rotation expected $($configuration.rotation), got $rotation"
        }
        $expectedWidth = if ($configuration.orientation -in @("portrait", "reverse_portrait")) {
            [Math]::Min($configuration.width, $configuration.height)
        } else {
            [Math]::Max($configuration.width, $configuration.height)
        }
        $expectedHeight = if ($configuration.orientation -in @("portrait", "reverse_portrait")) {
            [Math]::Max($configuration.width, $configuration.height)
        } else {
            [Math]::Min($configuration.width, $configuration.height)
        }
        $wmSize = Invoke-AdbShell $instance.adb_serial wm size
        $wmDensity = Invoke-AdbShell $instance.adb_serial wm density
        $displayDump = Invoke-AdbShell $instance.adb_serial dumpsys display
        if ($wmSize -notmatch "$expectedWidth\s*x\s*$expectedHeight") {
            throw "$($configuration.name) Android size expected ${expectedWidth}x${expectedHeight}, got $wmSize"
        }
        if ($wmDensity -notmatch "(?m)^Physical density:\s*$($configuration.dpi)\s*$") {
            throw "$($configuration.name) Android physical density expected $($configuration.dpi), got $wmDensity"
        }
        if ($displayDump -notmatch "(?m)^\s*DisplayDeviceInfo.*supportedModes.*fps=([0-9.]+)") {
            throw "$($configuration.name) Android active display mode was not reported"
        }
        $actualRefreshRate = [double]$Matches[1]
        if ([Math]::Abs($actualRefreshRate - $configuration.refresh_rate_hz) -gt 0.1) {
            throw "$($configuration.name) Android refresh rate expected $($configuration.refresh_rate_hz), got $actualRefreshRate"
        }
        $metricsPath = Join-Path $DataRoot "runs\$InstanceId\$($instance.active_run_id)\frame-metrics-v2.json"
        $metrics = Get-Content -Raw -LiteralPath $metricsPath | ConvertFrom-Json
        if ($metrics.generation -ne $instance.frame_generation -or
            $metrics.produced_frames -ne $metrics.imported_frames -or
            $metrics.imported_frames -ne $metrics.presented_frames -or
            $metrics.dropped_frames -ne 0 -or
            $metrics.cpu_readback_bytes -ne 0 -or
            $metrics.software_blit_count -ne 0) {
            throw "$($configuration.name) strict frame invariant failed"
        }
        $records.Add([ordered]@{
            name = $configuration.name
            run_id = $instance.active_run_id
            frame_generation = $instance.frame_generation
            display = $instance.spec.display
            android = [ordered]@{
                rotation = [int]$rotation
                wm_size = $wmSize
                wm_density = $wmDensity
                active_refresh_rate_hz = $actualRefreshRate
            }
            frame_metrics = $metrics
        })
        Invoke-Hdctl stop $InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
        Write-Host "display $($configuration.name) passed"
    }
} catch {
    $failure = $_.Exception.Message
} finally {
    try {
        Stop-IfRunning
        $restorePath = Join-Path $outputRoot "restore-spec.json"
        [IO.File]::WriteAllText($restorePath, $initialSpecJson, [Text.UTF8Encoding]::new($false))
        Invoke-Hdctl update $InstanceId.ToString() $restorePath | Out-Null
    } catch {
        if ($null -eq $failure) { $failure = "restore failed: $($_.Exception.Message)" }
    }
    $evidence = [ordered]@{
        schema_version = 1
        status = if ($null -eq $failure) { "pass" } else { "fail" }
        failure = $failure
        instance_id = $InstanceId
        guest_bundle_digest = $guestDigest
        host_bundle_digest = $hostDigest
        capability_and_bundle_revalidation = "skipped-by-design"
        completed = $records.Count
        configurations = $records
    }
    $evidencePath = Join-Path $outputRoot "windows-display-matrix.json"
    [IO.File]::WriteAllText($evidencePath, ($evidence | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    Write-Host "evidence=$evidencePath"
}

if ($null -ne $failure) { throw $failure }

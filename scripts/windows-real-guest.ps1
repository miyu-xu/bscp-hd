[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Guid]$InstanceId,

    [string]$DataRoot = "D:\hd-v2-data",

    [string]$Hdctl = "",

    [string]$Adb = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe",

    [ValidateRange(0, 1000)]
    [int]$LifecycleCycles = 0,

    [ValidateRange(0, 1440)]
    [int]$StabilityMinutes = 0,

    [ValidateRange(0, 4096)]
    [int]$MaxWorkerHandleGrowth = 64,

    [ValidateRange(0, 4096)]
    [int]$MaxRuntimeHandleGrowth = 128,

    [ValidateRange(0, 16384)]
    [int]$MaxWorkerPrivateGrowthMiB = 64,

    [ValidateRange(0, 16384)]
    [int]$MaxRuntimePrivateGrowthMiB = 1024,

    [ValidateRange(1, 4096)]
    [int]$MaxStabilityLogMiB = 256,

    [bool]$DevFastArtifacts = $true,

    [switch]$RunActions,

    [string]$Apk = "",

    [string]$SecondarySpec = "",

    [string]$Output = "out\windows-real-guest"
)

$ErrorActionPreference = "Stop"
if ($DevFastArtifacts) {
    $env:HD_DEV_FAST_ARTIFACTS = "1"
} else {
    Remove-Item Env:HD_DEV_FAST_ARTIFACTS -ErrorAction SilentlyContinue
}
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Hdctl)) {
    $Hdctl = Join-Path (Split-Path -Parent $repoRoot) "out\dist\windows\bin\hdctl.exe"
}
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $repoRoot $Output }
$evidencePath = Join-Path $outputRoot "windows-real-guest.json"
$startedAt = [DateTimeOffset]::UtcNow
$cycles = [Collections.Generic.List[object]]::new()
$samples = [Collections.Generic.List[object]]::new()
$actions = [Collections.Generic.List[string]]::new()
$secondaryId = $null

function Invoke-Hdctl {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & $script:Hdctl --data-root $script:DataRoot @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "hdctl $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join [Environment]::NewLine)
}

function Get-Instance {
    param([Guid]$Id)
    return (Invoke-Hdctl show $Id.ToString() | ConvertFrom-Json)
}

function Get-FrameMetrics {
    param($Instance)
    $path = Join-Path $script:DataRoot "runs\$($Instance.spec.id)\$($Instance.active_run_id)\frame-metrics-v2.json"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "frame metrics are missing: $path"
    }
    $metrics = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
    if ($metrics.generation -ne $Instance.frame_generation -or
        $metrics.produced_frames -ne $metrics.imported_frames -or
        $metrics.imported_frames -ne $metrics.presented_frames -or
        $metrics.dropped_frames -ne 0 -or
        $metrics.cpu_readback_bytes -ne 0 -or
        $metrics.software_blit_count -ne 0) {
        throw "strict frame invariant failed for run $($Instance.active_run_id)"
    }
    return $metrics
}

function Assert-Ready {
    param([Guid]$Id)
    $instance = Get-Instance $Id
    if ($instance.status.desired -ne "running" -or $instance.status.observed -ne "ready") {
        throw "instance $Id is not Ready: desired=$($instance.status.desired) observed=$($instance.status.observed)"
    }
    if ([string]::IsNullOrWhiteSpace($instance.adb_serial)) {
        throw "instance $Id has no ADB serial"
    }
    $boot = ((& $script:Adb -s $instance.adb_serial shell getprop sys.boot_completed 2>&1) -join "`n").Trim()
    if ($LASTEXITCODE -ne 0 -or $boot -ne "1") {
        throw "instance $Id boot readiness failed: $boot"
    }
    $packages = ((& $script:Adb -s $instance.adb_serial shell cmd package list packages android 2>&1) -join "`n").Trim()
    if ($LASTEXITCODE -ne 0 -or $packages -notmatch "package:android") {
        throw "instance $Id package manager readiness failed: $packages"
    }
    $metrics = Get-FrameMetrics $instance
    return [pscustomobject]@{ instance = $instance; metrics = $metrics }
}

function Assert-Stopped {
    param([Guid]$Id, [int]$WorkerPid)
    $instance = Get-Instance $Id
    if ($instance.status.observed -ne "stopped" -or
        $null -ne $instance.active_run_id -or
        $null -ne $instance.adb_serial) {
        throw "instance $Id did not persist the strict Stopped state"
    }
    if (-not (Get-Process -Id $WorkerPid -ErrorAction SilentlyContinue)) {
        throw "idle worker $WorkerPid exited unexpectedly after stop"
    }
    $runtimeChildren = Get-CimInstance Win32_Process -Filter "ParentProcessId=$WorkerPid" |
        Where-Object {
            $_.Name -eq "crosvm.exe" -or
            $_.Name -eq "hd-device-sim.exe" -or
            $_.Name -eq "hd-frame-producer.exe" -or
            $_.Name -eq "hd-adb-bridge.exe" -or
            $_.Name -like "hd-*-adapter.exe"
        }
    if ($runtimeChildren) {
        throw "runtime child processes remain below idle worker $WorkerPid"
    }
}

function Get-RuntimeProcessSnapshot {
    param([int]$WorkerPid)
    $processRows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name)
    $descendantIds = [Collections.Generic.HashSet[int]]::new()
    [void]$descendantIds.Add($WorkerPid)
    do {
        $added = $false
        foreach ($row in $processRows) {
            if ($descendantIds.Contains([int]$row.ParentProcessId) -and
                $descendantIds.Add([int]$row.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)

    $live = @($descendantIds | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
    if (-not ($live | Where-Object Id -eq $WorkerPid)) {
        throw "worker $WorkerPid disappeared while collecting the stability process snapshot"
    }
    return [pscustomobject]@{
        process_count = $live.Count
        handles = [long](($live | Measure-Object -Property HandleCount -Sum).Sum)
        private_bytes = [long](($live | Measure-Object -Property PrivateMemorySize64 -Sum).Sum)
        processes = @($live | Sort-Object Id | ForEach-Object {
            [ordered]@{
                pid = $_.Id
                name = $_.ProcessName
                handles = $_.HandleCount
                private_bytes = $_.PrivateMemorySize64
            }
        })
    }
}

function Get-StabilityLogSnapshot {
    param([Guid]$Id, [string]$RunId)
    $files = [Collections.Generic.List[IO.FileInfo]]::new()
    $workerLogRoot = Join-Path $script:DataRoot "logs\workers"
    if (Test-Path -LiteralPath $workerLogRoot -PathType Container) {
        Get-ChildItem -LiteralPath $workerLogRoot -File -Filter "$Id.jsonl*" -ErrorAction SilentlyContinue |
            ForEach-Object { $files.Add($_) }
    }
    $runRoot = Join-Path $script:DataRoot "runs\$Id\$RunId"
    if (Test-Path -LiteralPath $runRoot -PathType Container) {
        Get-ChildItem -LiteralPath $runRoot -File -Recurse -ErrorAction SilentlyContinue |
            ForEach-Object { $files.Add($_) }
    }
    return [pscustomobject]@{
        file_count = $files.Count
        bytes = [long](($files | Measure-Object -Property Length -Sum).Sum)
    }
}

function Invoke-Action {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    Invoke-Hdctl action $script:InstanceId.ToString() @Arguments | Out-Null
    $script:actions.Add(($Arguments -join ":"))
}

function Stop-IfRunning {
    param([Guid]$Id)
    $instance = Get-Instance $Id
    if ($instance.status.desired -eq "running") {
        Invoke-Hdctl stop $Id.ToString() --graceful-timeout-ms 20000 | Out-Null
    }
}

if (-not (Test-Path -LiteralPath $Hdctl -PathType Leaf)) { throw "hdctl is missing: $Hdctl" }
if (-not (Test-Path -LiteralPath $Adb -PathType Leaf)) { throw "adb is missing: $Adb" }
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null

$failure = $null
try {
    $initial = Get-Instance $InstanceId
    $guestDigest = $initial.spec.artifacts.guest_bundle_digest
    $hostDigest = $initial.spec.artifacts.host_bundle_digest

    if ($RunActions) {
        if ($initial.status.observed -ne "ready") {
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
        }
        Assert-Ready $InstanceId | Out-Null
        foreach ($orientation in @("portrait", "landscape", "reverse-portrait", "reverse-landscape", "portrait")) {
            Invoke-Action rotate $orientation
        }
        foreach ($key in @("home", "recent", "back", "volume-up", "volume-down", "power", "power")) {
            Invoke-Action key $key
        }
        Invoke-Action location 312304000 1214737000 --altitude-mm 4000 --accuracy-mm 3000
        Invoke-Action battery 73 --charging --temperature-deci-celsius 280
        Invoke-Action network 25 10 --bandwidth-kbps 50000
        Invoke-Action sensor accelerometer 0 0 9806650 --duration-ms 500
        Invoke-Action sensor gyroscope 1000 2000 3000 --duration-ms 500
        Invoke-Action sensor magnetometer 25000000 1000000 40000000 --duration-ms 500
        Invoke-Action sensor light 42000000 --duration-ms 500
        Invoke-Action sensor proximity 5000000 --duration-ms 500
        $peer = [Guid]::NewGuid()
        Invoke-Action bluetooth-create $peer.ToString() HD-Acceptance-Peer
        Invoke-Action bluetooth-advertise $peer.ToString() true
        Invoke-Action bluetooth-advertise $peer.ToString() false
        Invoke-Action bluetooth-remove $peer.ToString()
        Invoke-Action nfc-type2 D101055401656E6869
        Invoke-Action nfc-remove
        Invoke-Action nfc-type4 D101055401656E6869
        Invoke-Action nfc-remove
        if (-not [string]::IsNullOrWhiteSpace($Apk)) {
            if (-not (Test-Path -LiteralPath $Apk -PathType Leaf)) { throw "APK is missing: $Apk" }
            Invoke-Hdctl install $InstanceId.ToString() $Apk | Out-Null
            $packagePath = ((& $Adb -s (Get-Instance $InstanceId).adb_serial shell cmd package path com.hd.acceptance 2>&1) -join "`n").Trim()
            if ($LASTEXITCODE -ne 0 -or $packagePath -notmatch "^package:") {
                throw "installed acceptance package was not read back: $packagePath"
            }
            $actions.Add("install:com.hd.acceptance")
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($SecondarySpec)) {
        if (-not (Test-Path -LiteralPath $SecondarySpec -PathType Leaf)) { throw "secondary spec is missing: $SecondarySpec" }
        $secondaryId = [Guid](Get-Content -Raw -LiteralPath $SecondarySpec | ConvertFrom-Json).id
        Invoke-Hdctl create --spec $SecondarySpec | Out-Null
        Invoke-Hdctl start $secondaryId.ToString() | Out-Null
        $primaryReady = Assert-Ready $InstanceId
        $secondaryReady = Assert-Ready $secondaryId
        if ($primaryReady.instance.worker.pid -eq $secondaryReady.instance.worker.pid -or
            $primaryReady.instance.adb_serial -eq $secondaryReady.instance.adb_serial -or
            $primaryReady.instance.active_run_id -eq $secondaryReady.instance.active_run_id) {
            throw "dual-instance identity or endpoint isolation failed"
        }
        Invoke-Hdctl stop $secondaryId.ToString() --graceful-timeout-ms 20000 | Out-Null
        Invoke-Hdctl delete $secondaryId.ToString() | Out-Null
        $secondaryId = $null
        $actions.Add("dual-instance")
    }

    if ($LifecycleCycles -gt 0) {
        Stop-IfRunning $InstanceId
        for ($cycle = 1; $cycle -le $LifecycleCycles; $cycle++) {
            $cycleStarted = [DateTimeOffset]::UtcNow
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
            $ready = Assert-Ready $InstanceId
            $workerPid = [int]$ready.instance.worker.pid
            Invoke-Hdctl stop $InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
            Assert-Stopped $InstanceId $workerPid
            $cycles.Add([ordered]@{
                cycle = $cycle
                run_id = $ready.instance.active_run_id
                frame_generation = $ready.instance.frame_generation
                worker_pid = $workerPid
                presented_frames = $ready.metrics.presented_frames
                duration_ms = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
            })
            Write-Host "lifecycle $cycle/$LifecycleCycles passed"
        }
    }

    if ($StabilityMinutes -gt 0) {
        $instance = Get-Instance $InstanceId
        if ($instance.status.observed -ne "ready") {
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
        }
        $stable = Assert-Ready $InstanceId
        $stableRun = $stable.instance.active_run_id
        $stableWorkerPid = [int]$stable.instance.worker.pid
        $lastPresentedFrames = [long]$stable.metrics.presented_frames
        $deadline = [DateTimeOffset]::UtcNow.AddMinutes($StabilityMinutes)
        while ([DateTimeOffset]::UtcNow -lt $deadline) {
            Start-Sleep -Seconds 30
            $sample = Assert-Ready $InstanceId
            if ($sample.instance.active_run_id -ne $stableRun) {
                throw "stability run changed from $stableRun to $($sample.instance.active_run_id)"
            }
            if ([int]$sample.instance.worker.pid -ne $stableWorkerPid) {
                throw "stability worker changed from $stableWorkerPid to $($sample.instance.worker.pid)"
            }
            $lastPresentedFrames = [long]$sample.metrics.presented_frames
            $workerProcess = Get-Process -Id $stableWorkerPid
            $runtimeProcesses = Get-RuntimeProcessSnapshot $stableWorkerPid
            if (-not ($runtimeProcesses.processes | Where-Object name -eq "hd-frame-producer")) {
                throw "frame producer is missing from the live runtime process tree"
            }
            if (-not ($runtimeProcesses.processes | Where-Object name -eq "crosvm")) {
                throw "crosvm is missing from the live runtime process tree"
            }
            $logs = Get-StabilityLogSnapshot $InstanceId $stableRun
            $samples.Add([ordered]@{
                timestamp = [DateTimeOffset]::UtcNow.ToString("O")
                presented_frames = $sample.metrics.presented_frames
                worker_handles = $workerProcess.HandleCount
                worker_private_bytes = $workerProcess.PrivateMemorySize64
                runtime_process_count = $runtimeProcesses.process_count
                runtime_handles = $runtimeProcesses.handles
                runtime_private_bytes = $runtimeProcesses.private_bytes
                runtime_processes = $runtimeProcesses.processes
                log_file_count = $logs.file_count
                log_bytes = $logs.bytes
            })
            Write-Host "stability sample $($samples.Count) passed"
        }

        if ($samples.Count -lt 2) {
            throw "stability validation produced fewer than two samples"
        }
        $firstSample = $samples[0]
        $lastSample = $samples[$samples.Count - 1]
        $workerHandleGrowth = [long]$lastSample.worker_handles - [long]$firstSample.worker_handles
        $runtimeHandleGrowth = [long]$lastSample.runtime_handles - [long]$firstSample.runtime_handles
        $workerPrivateGrowth = [long]$lastSample.worker_private_bytes - [long]$firstSample.worker_private_bytes
        $runtimePrivateGrowth = [long]$lastSample.runtime_private_bytes - [long]$firstSample.runtime_private_bytes
        if ([long]$lastSample.presented_frames -le [long]$firstSample.presented_frames) {
            throw "presented frames did not advance across the stability interval"
        }
        if ($workerHandleGrowth -gt $MaxWorkerHandleGrowth) {
            throw "worker handle growth $workerHandleGrowth exceeds limit $MaxWorkerHandleGrowth"
        }
        if ($runtimeHandleGrowth -gt $MaxRuntimeHandleGrowth) {
            throw "runtime handle growth $runtimeHandleGrowth exceeds limit $MaxRuntimeHandleGrowth"
        }
        if ($workerPrivateGrowth -gt ([long]$MaxWorkerPrivateGrowthMiB * 1MB)) {
            throw "worker private-byte growth $workerPrivateGrowth exceeds limit $MaxWorkerPrivateGrowthMiB MiB"
        }
        if ($runtimePrivateGrowth -gt ([long]$MaxRuntimePrivateGrowthMiB * 1MB)) {
            throw "runtime private-byte growth $runtimePrivateGrowth exceeds limit $MaxRuntimePrivateGrowthMiB MiB"
        }
        if ([int]$lastSample.runtime_process_count -ne [int]$firstSample.runtime_process_count) {
            throw "runtime process count changed from $($firstSample.runtime_process_count) to $($lastSample.runtime_process_count)"
        }
        if ([long]$lastSample.log_bytes -gt ([long]$MaxStabilityLogMiB * 1MB)) {
            throw "stability logs reached $($lastSample.log_bytes) bytes and exceed limit $MaxStabilityLogMiB MiB"
        }
    }
} catch {
    $failure = $_.Exception.Message
} finally {
    if ($null -ne $secondaryId) {
        try {
            $secondary = Get-Instance $secondaryId
            if ($secondary.status.desired -eq "running") {
                Invoke-Hdctl stop $secondaryId.ToString() --graceful-timeout-ms 20000 | Out-Null
            } else {
                $deadline = [DateTimeOffset]::UtcNow.AddMinutes(2)
                while ($secondary.status.observed -ne "stopped" -and [DateTimeOffset]::UtcNow -lt $deadline) {
                    Start-Sleep -Milliseconds 250
                    $secondary = Get-Instance $secondaryId
                }
            }
            Invoke-Hdctl delete $secondaryId.ToString() | Out-Null
        } catch { }
    }
    $evidence = [ordered]@{
        schema_version = 2
        platform = "windows"
        architecture = "x86_64"
        generated_at = [DateTimeOffset]::UtcNow.ToString("O")
        started_at = $startedAt.ToString("O")
        status = if ($null -eq $failure) { "pass" } else { "fail" }
        error = $failure
        instance_id = $InstanceId.ToString()
        guest_bundle_digest = $guestDigest
        host_bundle_digest = $hostDigest
        capability_and_bundle_revalidation = if ($DevFastArtifacts) {
            "skipped-by-design"
        } else {
            "performed-by-host"
        }
        dev_fast_artifacts = $DevFastArtifacts
        actions = $actions
        lifecycle = [ordered]@{ requested = $LifecycleCycles; completed = $cycles.Count; cycles = $cycles }
        stability = [ordered]@{ requested_minutes = $StabilityMinutes; completed_samples = $samples.Count; samples = $samples }
    }
    $evidence | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
    Write-Host "evidence=$evidencePath"
}

if ($null -ne $failure) { throw $failure }

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Guid]$InstanceId,

    [string]$DataRoot = "D:\hd-v2-data",

    [string]$Hdctl = "C:\workspace\bscp\bscp\out\dist\windows\bin\hdctl.exe",

    [string]$Adb = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe",

    [ValidateRange(1, 15)]
    [int]$RecoveryTimeoutMinutes = 7,

    [bool]$DevFastArtifacts = $true,

    [string]$Output = "out\windows-fault-injection"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $repoRoot $Output }
$evidencePath = Join-Path $outputRoot "windows-fault-injection.json"
$startedAt = [DateTimeOffset]::UtcNow
$scenarios = [Collections.Generic.List[object]]::new()
$failure = $null

if ($DevFastArtifacts) {
    $env:HD_DEV_FAST_ARTIFACTS = "1"
}

function Invoke-Hdctl {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & $script:Hdctl --data-root $script:DataRoot @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "hdctl $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join [Environment]::NewLine)
}

function Get-Instance {
    return (Invoke-Hdctl show $script:InstanceId.ToString() | ConvertFrom-Json)
}

function Get-FrameMetrics {
    param($Instance)
    $path = Join-Path $script:DataRoot "runs\$($Instance.spec.id)\$($Instance.active_run_id)\frame-metrics-v2.json"
    $metrics = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
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
    param($Instance)
    if ($Instance.status.desired -ne "running" -or $Instance.status.observed -ne "ready") {
        throw "instance is not Ready: desired=$($Instance.status.desired) observed=$($Instance.status.observed)"
    }
    if ([string]::IsNullOrWhiteSpace($Instance.adb_serial)) {
        throw "Ready instance has no ADB serial"
    }
    $boot = ((& $script:Adb -s $Instance.adb_serial shell getprop sys.boot_completed 2>&1) -join "`n").Trim()
    if ($LASTEXITCODE -ne 0 -or $boot -ne "1") {
        throw "ADB boot readiness failed: $boot"
    }
    Get-FrameMetrics $Instance | Out-Null
}

function Get-DirectChild {
    param([int]$ParentPid, [string]$Name)
    $process = Get-CimInstance Win32_Process -Filter "ParentProcessId=$ParentPid" |
        Where-Object Name -eq $Name |
        Select-Object -First 1
    if ($null -eq $process) {
        throw "$Name is not a direct child of $ParentPid"
    }
    return $process
}

function Wait-ReplacementReady {
    param(
        [string]$PreviousRunId,
        [long]$PreviousGeneration,
        [int]$PreviousWorkerPid,
        [bool]$RequireNewWorker
    )
    $deadline = [DateTimeOffset]::UtcNow.AddMinutes($script:RecoveryTimeoutMinutes)
    $transitions = [Collections.Generic.List[object]]::new()
    $last = ""
    while ([DateTimeOffset]::UtcNow -lt $deadline) {
        $instance = Get-Instance
        $state = "$($instance.status.observed)|$($instance.status.error_code)|$($instance.worker.pid)|$($instance.active_run_id)|$($instance.frame_generation)"
        if ($state -ne $last) {
            $transitions.Add([ordered]@{
                timestamp = [DateTimeOffset]::UtcNow.ToString("O")
                observed = $instance.status.observed
                error_code = $instance.status.error_code
                worker_pid = $instance.worker.pid
                run_id = $instance.active_run_id
                frame_generation = $instance.frame_generation
            })
            Write-Host $state
            $last = $state
        }
        $workerMatches = if ($RequireNewWorker) {
            $null -ne $instance.worker -and [int]$instance.worker.pid -ne $PreviousWorkerPid
        } else {
            $null -ne $instance.worker -and [int]$instance.worker.pid -eq $PreviousWorkerPid
        }
        if ($instance.status.observed -eq "ready" -and
            $instance.active_run_id -ne $PreviousRunId -and
            [long]$instance.frame_generation -gt $PreviousGeneration -and
            $workerMatches) {
            Assert-Ready $instance
            return [pscustomobject]@{ instance = $instance; transitions = $transitions }
        }
        if ($instance.status.observed -eq "blocked") {
            throw "recovery entered Blocked: $($instance.status.error_code) $($instance.status.reason)"
        }
        Start-Sleep -Milliseconds 500
    }
    throw "replacement runtime did not reach Ready before the recovery deadline"
}

function Add-RuntimeExitScenario {
    param([string]$Name, [string]$ChildName)
    $before = Get-Instance
    Assert-Ready $before
    $workerPid = [int]$before.worker.pid
    $child = Get-DirectChild $workerPid $ChildName
    $scenarioStarted = [DateTimeOffset]::UtcNow
    Stop-Process -Id ([int]$child.ProcessId) -Force
    $recovered = Wait-ReplacementReady `
        -PreviousRunId $before.active_run_id `
        -PreviousGeneration ([long]$before.frame_generation) `
        -PreviousWorkerPid $workerPid `
        -RequireNewWorker $false
    $script:scenarios.Add([ordered]@{
        name = $Name
        injected_pid = [int]$child.ProcessId
        previous_worker_pid = $workerPid
        recovered_worker_pid = [int]$recovered.instance.worker.pid
        previous_run_id = $before.active_run_id
        recovered_run_id = $recovered.instance.active_run_id
        previous_frame_generation = [long]$before.frame_generation
        recovered_frame_generation = [long]$recovered.instance.frame_generation
        duration_ms = [long]([DateTimeOffset]::UtcNow - $scenarioStarted).TotalMilliseconds
        transitions = $recovered.transitions
        status = "pass"
    })
}

if (-not (Test-Path -LiteralPath $Hdctl -PathType Leaf)) { throw "hdctl is missing: $Hdctl" }
if (-not (Test-Path -LiteralPath $Adb -PathType Leaf)) { throw "adb is missing: $Adb" }
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null

try {
    $initial = Get-Instance
    $guestDigest = $initial.spec.artifacts.guest_bundle_digest
    $hostDigest = $initial.spec.artifacts.host_bundle_digest
    if ($initial.status.observed -ne "ready") {
        if ($initial.status.desired -eq "running") {
            Invoke-Hdctl stop $InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
        }
        Invoke-Hdctl start $InstanceId.ToString() | Out-Null
    }
    Assert-Ready (Get-Instance)

    Add-RuntimeExitScenario "frame-producer-exit" "hd-frame-producer.exe"
    Add-RuntimeExitScenario "crosvm-exit" "crosvm.exe"

    $beforeWorkerExit = Get-Instance
    Assert-Ready $beforeWorkerExit
    $workerPid = [int]$beforeWorkerExit.worker.pid
    $workerScenarioStarted = [DateTimeOffset]::UtcNow
    Stop-Process -Id $workerPid -Force
    $workerRecovered = Wait-ReplacementReady `
        -PreviousRunId $beforeWorkerExit.active_run_id `
        -PreviousGeneration ([long]$beforeWorkerExit.frame_generation) `
        -PreviousWorkerPid $workerPid `
        -RequireNewWorker $true
    $scenarios.Add([ordered]@{
        name = "worker-exit"
        injected_pid = $workerPid
        previous_worker_pid = $workerPid
        recovered_worker_pid = [int]$workerRecovered.instance.worker.pid
        previous_run_id = $beforeWorkerExit.active_run_id
        recovered_run_id = $workerRecovered.instance.active_run_id
        previous_frame_generation = [long]$beforeWorkerExit.frame_generation
        recovered_frame_generation = [long]$workerRecovered.instance.frame_generation
        duration_ms = [long]([DateTimeOffset]::UtcNow - $workerScenarioStarted).TotalMilliseconds
        transitions = $workerRecovered.transitions
        status = "pass"
    })

    $beforeHostExit = Get-Instance
    Assert-Ready $beforeHostExit
    $hostPid = [int](Invoke-Hdctl health | ConvertFrom-Json).pid
    $hostScenarioStarted = [DateTimeOffset]::UtcNow
    Stop-Process -Id $hostPid -Force
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        if (-not (Get-Process -Id $hostPid -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 250
    }
    if (Get-Process -Id $hostPid -ErrorAction SilentlyContinue) {
        throw "injected Host process $hostPid did not exit"
    }
    if (-not (Get-Process -Id ([int]$beforeHostExit.worker.pid) -ErrorAction SilentlyContinue)) {
        throw "Worker exited with the Host"
    }
    $crosvm = Get-DirectChild ([int]$beforeHostExit.worker.pid) "crosvm.exe"
    $hostExecutable = Join-Path (Split-Path -Parent $Hdctl) "hd-host.exe"
    if (-not (Test-Path -LiteralPath $hostExecutable -PathType Leaf)) {
        throw "hd-host is missing beside hdctl: $hostExecutable"
    }
    $newHostProcess = Start-Process `
        -FilePath $hostExecutable `
        -ArgumentList @("--data-root", $DataRoot) `
        -WorkingDirectory (Split-Path -Parent $hostExecutable) `
        -WindowStyle Hidden `
        -PassThru
    $health = $null
    for ($attempt = 1; $attempt -le 80; $attempt++) {
        try {
            $health = Invoke-Hdctl @("--no-start-host", "health") | ConvertFrom-Json
            break
        } catch {
            if ($attempt -eq 80) { throw }
            Start-Sleep -Milliseconds 250
        }
    }
    if ([int]$health.pid -ne $newHostProcess.Id) {
        throw "health identity $($health.pid) does not match explicitly started Host $($newHostProcess.Id)"
    }
    Write-Host "host restarted pid=$($health.pid)"
    $afterHostExit = $null
    $hostReconnected = $false
    $lastHostState = ""
    for ($attempt = 1; $attempt -le 240; $attempt++) {
        try {
            $afterHostExit = Get-Instance
            $hostState = "$($afterHostExit.status.observed)|$($afterHostExit.worker.pid)|$($afterHostExit.active_run_id)"
            if ($hostState -ne $lastHostState) {
                Write-Host $hostState
                $lastHostState = $hostState
            }
            if ($afterHostExit.status.observed -eq "ready" -and
                [int]$afterHostExit.worker.pid -eq [int]$beforeHostExit.worker.pid -and
                $afterHostExit.active_run_id -eq $beforeHostExit.active_run_id) {
                $hostReconnected = $true
                break
            }
        } catch {
            if ($attempt -eq 240) { throw }
        }
        Start-Sleep -Milliseconds 500
    }
    if (-not $hostReconnected) {
        throw "new Host did not reconnect the surviving Worker and run within 120 seconds"
    }
    Assert-Ready $afterHostExit
    if ([int]$afterHostExit.worker.pid -ne [int]$beforeHostExit.worker.pid -or
        $afterHostExit.active_run_id -ne $beforeHostExit.active_run_id) {
        throw "Host recovery restarted the surviving Worker or Guest"
    }
    $scenarios.Add([ordered]@{
        name = "host-exit"
        injected_pid = $hostPid
        recovered_host_pid = [int]$health.pid
        surviving_worker_pid = [int]$afterHostExit.worker.pid
        surviving_crosvm_pid = [int]$crosvm.ProcessId
        run_id = $afterHostExit.active_run_id
        frame_generation = [long]$afterHostExit.frame_generation
        duration_ms = [long]([DateTimeOffset]::UtcNow - $hostScenarioStarted).TotalMilliseconds
        status = "pass"
    })
} catch {
    $failure = $_.Exception.Message
} finally {
    try {
        $final = Get-Instance
        if ($final.status.desired -eq "running") {
            Invoke-Hdctl stop $InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
        }
    } catch {
        if ($null -eq $failure) { $failure = "cleanup failed: $($_.Exception.Message)" }
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
        capability_and_bundle_revalidation = "skipped-by-design"
        dev_fast_artifacts = $DevFastArtifacts
        scenarios = $scenarios
    }
    $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
    Write-Host "evidence=$evidencePath"
}

if ($null -ne $failure) { throw $failure }

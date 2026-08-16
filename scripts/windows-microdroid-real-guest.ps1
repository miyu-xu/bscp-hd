[CmdletBinding()]
param(
    [string]$ArtifactRoot = "D:\bscp\bscp-vm-artifacts-20260721-aosp-all-targets\products\microdroid\vsoc_x86_64",
    [string]$DistRoot = "",
    [string]$Hdctl = "",
    [string]$Adb = "",
    [ValidateSet("loopback", "disabled")]
    [string]$AdbMode = "loopback",
    [ValidateSet("full", "none")]
    [string]$DebugLevel = "full",
    [ValidateSet("one_cpu", "match_host")]
    [string]$CpuTopology = "one_cpu",
    [ValidateRange(256, 1048576)]
    [int]$MemoryMiB = 512,
    [ValidateRange(0, 4096)]
    [int]$EncryptedStorageMiB = 64,
    [string]$DataRoot = "",
    [string]$Output = "out\windows-microdroid-real-guest",
    [string]$PayloadApk = "",
    [string]$ExtraApkMaterials = "",
    [Nullable[int]]$ExpectedPayloadExitCode = $null,
    [ValidateRange(1, 100)]
    [int]$Cycles = 1,
    [ValidateRange(5, 180)]
    [int]$ReadyTimeoutSeconds = 90
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $repoRoot
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $repoRoot $Output }
if ([string]::IsNullOrWhiteSpace($DistRoot)) {
    $DistRoot = Join-Path $workspaceRoot "out\dist"
}
if ([string]::IsNullOrWhiteSpace($Hdctl)) {
    $Hdctl = Join-Path $DistRoot "windows\bin\hdctl.exe"
}
if ([string]::IsNullOrWhiteSpace($Adb)) {
    $Adb = Join-Path $env:LOCALAPPDATA "Android\Sdk\platform-tools\adb.exe"
}
if ([string]::IsNullOrWhiteSpace($DataRoot)) {
    $DataRoot = Join-Path $outputRoot "data"
}
$payloadMode = if ([string]::IsNullOrWhiteSpace($PayloadApk)) { "empty" } else { "uploaded" }
$extraMaterial = $null
if (-not [string]::IsNullOrWhiteSpace($ExtraApkMaterials)) {
    $ExtraApkMaterials = (Resolve-Path -LiteralPath $ExtraApkMaterials -ErrorAction Stop).Path
    $materialManifest = Join-Path $ExtraApkMaterials "materials.json"
    if (-not (Test-Path -LiteralPath $materialManifest -PathType Leaf)) {
        throw "extra APK material directory omits materials.json"
    }
    $extraMaterial = Get-Content -Raw -LiteralPath $materialManifest | ConvertFrom-Json
    if ($extraMaterial.profile -ne "hd-microdroid-extra-apk-qa-materials-v1" -or
        $extraMaterial.main_payload.declared_extra_apks -ne 2 -or
        @($extraMaterial.extra_apks).Count -ne 2 -or $extraMaterial.private_key_retained) {
        throw "extra APK material manifest violates the QA profile"
    }
    $PayloadApk = Join-Path $ExtraApkMaterials $extraMaterial.main_payload.file
    $payloadMode = "uploaded"
}
$finitePayloadMode = $null -ne $ExpectedPayloadExitCode
if ($payloadMode -eq "uploaded") {
    if (-not (Test-Path -LiteralPath $PayloadApk -PathType Leaf)) {
        throw "Microdroid Payload APK is missing: $PayloadApk"
    }
    $PayloadApk = (Resolve-Path -LiteralPath $PayloadApk).Path
}
if ($null -ne $extraMaterial) {
    if ((Get-FileHash -LiteralPath $PayloadApk -Algorithm SHA256).Hash.ToLowerInvariant() -ne
        $extraMaterial.main_payload.sha256) {
        throw "extra APK main Payload SHA-256 does not match materials.json"
    }
    foreach ($entry in @($extraMaterial.extra_apks)) {
        $path = Join-Path $ExtraApkMaterials $entry.file
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or
            (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) {
            throw "extra APK material does not match materials.json: $($entry.file)"
        }
    }
}
if ($finitePayloadMode) {
    if ($payloadMode -ne "uploaded") {
        throw "finite Payload completion requires -PayloadApk"
    }
    if ($Cycles -ne 1) {
        throw "finite Payload completion evidence requires exactly one cycle per APK"
    }
    if ($AdbMode -ne "disabled") {
        throw "finite Payload completion must not race the independent ADB readiness probe; use -AdbMode disabled"
    }
}
if ($null -ne $extraMaterial -and $finitePayloadMode) {
    throw "extra APK real-Guest mode cannot be combined with finite Payload completion"
}
if ($DebugLevel -eq "none" -and $AdbMode -ne "disabled") {
    throw "Microdroid debug=none must disable ADB"
}
if ($EncryptedStorageMiB -in 1..9) {
    throw "Microdroid encrypted storage must be 0 (disabled) or 10..=4096 MiB"
}
if ($null -ne $extraMaterial -and ($DebugLevel -ne "full" -or $AdbMode -ne "loopback")) {
    throw "extra APK Guest verification requires Full debug with loopback ADB"
}

$requiredArtifacts = @(
    "apex_dir\apex\com.android.virt\etc\microdroid.json",
    "apex_dir\apex\com.android.virt\etc\fs\microdroid_kernel",
    "apex_dir\apex\com.android.virt\etc\fs\microdroid_super.img",
    "apex_dir\apex\com.android.virt\etc\microdroid_initrd_debuggable.img",
    "apex_dir\apex\com.android.virt\etc\microdroid_initrd_normal.img"
)
$requiredRuntime = @(
    "windows\bin\hdctl.exe",
    "windows\bin\hd-host.exe",
    "windows\bin\hd-worker.exe",
    "windows\bin\vm.exe",
    "windows\bin\virtmgr.exe",
    "windows\bin\crosvm.exe",
    "windows\bin\libbinder-rpc.dll"
)
foreach ($relative in $requiredArtifacts) {
    $path = Join-Path $ArtifactRoot $relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Microdroid artifact is missing: $path"
    }
}
foreach ($relative in $requiredRuntime) {
    $path = Join-Path $DistRoot $relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Windows Microdroid runtime is missing: $path"
    }
}
if ($AdbMode -eq "loopback" -and -not (Test-Path -LiteralPath $Adb -PathType Leaf)) {
    throw "Windows Microdroid ADB client is missing: $Adb"
}

New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
if (Test-Path -LiteralPath $DataRoot) {
    $existing = @(Get-ChildItem -LiteralPath $DataRoot -Force -ErrorAction Stop)
    if ($existing.Count -ne 0) {
        throw "refusing to reuse a non-empty Microdroid smoke data root: $DataRoot"
    }
} else {
    New-Item -ItemType Directory -Path $DataRoot -Force | Out-Null
}

$env:HD_MICRODROID_DIST_ROOT = $DistRoot
$env:HD_MICRODROID_ARTIFACTS_ROOT = $ArtifactRoot
$env:HD_MICRODROID_DEV_BYPASS = "1"
$instanceId = [Guid]::NewGuid()
$specPath = Join-Path $outputRoot "microdroid-spec.json"
$evidencePath = Join-Path $outputRoot "result.json"
$cycleEvidence = [Collections.Generic.List[object]]::new()
$startedAt = [DateTimeOffset]::UtcNow
$created = $false
$failure = $null
$failureStack = $null
$hostProcess = $null
$hostStdout = Join-Path $outputRoot "hd-host.stdout.txt"
$hostStderr = Join-Path $outputRoot "hd-host.stderr.txt"
$payloadEvidence = $null

function Invoke-Hdctl {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $lines = & $script:Hdctl --data-root $script:DataRoot --no-start-host @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "hdctl $($Arguments -join ' ') failed: $($lines -join [Environment]::NewLine)"
    }
    return ($lines -join [Environment]::NewLine)
}

function Assert-LaunchArgumentValue {
    param(
        [object[]]$LaunchArguments,
        [string]$Name,
        [string]$Expected
    )
    $matches = [Collections.Generic.List[int]]::new()
    for ($index = 0; $index -lt $LaunchArguments.Count; $index++) {
        if ([string]$LaunchArguments[$index] -eq $Name) {
            $matches.Add($index)
        }
    }
    if ($matches.Count -ne 1 -or $matches[0] + 1 -ge $LaunchArguments.Count -or
        [string]$LaunchArguments[$matches[0] + 1] -ne $Expected) {
        throw "launch manifest must contain exactly one '$Name $Expected' pair"
    }
}

function Start-IsolatedHost {
    $existing = @(Get-ScopedRuntimeProcesses)
    if ($existing.Count -ne 0) {
        throw "isolated data root already has running HD processes"
    }
    $hostExecutable = Join-Path $script:DistRoot "windows\bin\hd-host.exe"
    $argumentLine = "--data-root `"$($script:DataRoot.Replace('"', '\"'))`""
    $startParameters = @{
        FilePath = $hostExecutable
        ArgumentList = $argumentLine
        WorkingDirectory = Split-Path -Parent $hostExecutable
        RedirectStandardOutput = $script:hostStdout
        RedirectStandardError = $script:hostStderr
        WindowStyle = "Hidden"
        PassThru = $true
    }
    $script:hostProcess = Start-Process @startParameters
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
    do {
        if ($script:hostProcess.HasExited) {
            $stderr = if (Test-Path -LiteralPath $script:hostStderr) {
                Get-Content -Raw -LiteralPath $script:hostStderr
            } else { "" }
            throw "isolated HD Host exited during startup: $stderr"
        }
        try {
            $health = Invoke-Hdctl health | ConvertFrom-Json
            if ($health.pid -eq $script:hostProcess.Id) {
                return
            }
        } catch {
            Start-Sleep -Milliseconds 100
        }
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "isolated HD Host did not become healthy within 15 seconds"
}

function Get-Instance {
    return (Invoke-Hdctl show $script:instanceId.ToString() | ConvertFrom-Json)
}

function Invoke-Adb {
    param(
        [int]$TimeoutSeconds = 10,
        [Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $script:Adb
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) {
        [void]$startInfo.ArgumentList.Add($argument)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "failed to start adb"
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill($true) } catch { }
            [void]$process.WaitForExit(2000)
            throw "adb $($Arguments -join ' ') timed out after $TimeoutSeconds seconds"
        }
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            $detail = @($stdout.Trim(), $stderr.Trim()) | Where-Object { $_ }
            throw "adb $($Arguments -join ' ') failed with exit code $($process.ExitCode): $($detail -join [Environment]::NewLine)"
        }
        return $stdout.Trim()
    } finally {
        $process.Dispose()
    }
}

function Wait-ForAdbReady {
    param([int]$TimeoutSeconds)
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $instance = Get-Instance
        if ($instance.adb_ready) {
            if ([string]::IsNullOrWhiteSpace($instance.adb_serial)) {
                throw "Microdroid reported ADB Ready without a serial"
            }
            return $instance
        }
        if ($instance.status.observed -notin @("ready", "starting", "guest_booting", "adb_connecting")) {
            throw "Microdroid left the ADB readiness path in state $($instance.status.observed)"
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "Microdroid did not publish ADB Ready within $TimeoutSeconds seconds"
}

function Wait-ForObservedState {
    param([string]$Expected, [int]$TimeoutSeconds)
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $instance = Get-Instance
        if ($instance.status.observed -eq $Expected) {
            return $instance
        }
        if ($instance.status.observed -in @("blocked", "failed")) {
            $detail = if ($null -ne $instance.status.error_code -or
                $null -ne $instance.status.reason) {
                "$($instance.status.error_code): $($instance.status.reason)"
            } else {
                "no structured error"
            }
            throw "Microdroid entered $($instance.status.observed): $detail"
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "Microdroid did not reach $Expected within $TimeoutSeconds seconds"
}

function Get-ScopedRuntimeProcesses {
    $needle = $script:DataRoot.ToLowerInvariant()
    return @(Get-CimInstance Win32_Process | Where-Object {
        $_.CommandLine -and $_.CommandLine.ToLowerInvariant().Contains($needle) -and
        $_.Name -in @("hd-host.exe", "hd-worker.exe", "vm.exe", "virtmgr.exe", "crosvm.exe")
    } | Select-Object ProcessId, ParentProcessId, Name, CommandLine)
}

function Get-ActiveRuntimeProcesses {
    if ($null -eq $script:hostProcess -or $script:hostProcess.HasExited) {
        return @()
    }
    $rows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, CommandLine)
    $ids = [Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($script:hostProcess.Id)
    do {
        $added = $false
        foreach ($row in $rows) {
            if ($ids.Contains([int]$row.ParentProcessId) -and $ids.Add([int]$row.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)
    return @($rows | Where-Object {
        $ids.Contains([int]$_.ProcessId) -and
        $_.Name -in @("hd-host.exe", "hd-worker.exe", "vm.exe", "virtmgr.exe", "crosvm.exe")
    })
}

function Save-FailureRunEvidence {
    $source = Join-Path $script:DataRoot "runs\$($script:instanceId)"
    if (-not (Test-Path -LiteralPath $source -PathType Container)) {
        return
    }
    $destination = Join-Path $script:outputRoot "failed-runs"
    if (Test-Path -LiteralPath $destination) {
        throw "refusing to replace existing failure evidence: $destination"
    }
    Copy-Item -LiteralPath $source -Destination $destination -Recurse
}

$spec = [ordered]@{
    schema_version = 2
    id = $instanceId.ToString()
    name = "Windows AOSP Microdroid Smoke"
    guest_kind = "microdroid"
    microdroid = [ordered]@{
        debug_level = $DebugLevel
        cpu_topology = $CpuTopology
        payload = [ordered]@{ kind = "empty" }
        payload_extra_apk_count = $null
        extra_apks = @()
        encrypted_storage_mib = if ($EncryptedStorageMiB -eq 0) { $null } else { $EncryptedStorageMiB }
    }
    cpu_count = 1
    memory_mib = $MemoryMiB
    display = [ordered]@{
        width = 1080
        height = 1920
        dpi = 420
        refresh_rate_hz = 60
        orientation = "portrait"
        vsync = "on"
        show_host_fps = $false
        secondary_displays = @()
    }
    adb = if ($AdbMode -eq "loopback") {
        [ordered]@{ mode = "loopback"; host_port = $null; executable = (Resolve-Path -LiteralPath $Adb).Path }
    } else {
        [ordered]@{ mode = "disabled"; host_port = $null; executable = $null }
    }
    artifacts = $null
    boot = [ordered]@{ kernel_log_level = 4; panic_timeout_seconds = 5; boot_animation = $true }
    devices = [ordered]@{
        bluetooth = $false; nfc = $false; uwb = $false; modem = $false
        gnss = $false; sensors = $false; network = $false; audio = $false
        camera = $false; power = $false; touchpad = $false
    }
    host_audio_input = "disabled"
    restart_policy = "never"
    labels = [ordered]@{ gate = "windows-microdroid-real-guest" }
}
try {
    Start-IsolatedHost
    if ($payloadMode -eq "uploaded") {
        $sourceSha256 = (Get-FileHash -LiteralPath $PayloadApk -Algorithm SHA256).Hash.ToLowerInvariant()
        $upload = Invoke-Hdctl upload --microdroid-payload $PayloadApk | ConvertFrom-Json
        if ($upload.sha256 -ne $sourceSha256 -or $null -eq $upload.id) {
            throw "Host upload identity does not match the selected Payload APK"
        }
        $spec.microdroid.payload = [ordered]@{
            kind = "uploaded"
            upload_id = $upload.id
            sha256 = $upload.sha256
            config_path = "assets/vm_config.json"
        }
        $payloadEvidence = [ordered]@{
            kind = "uploaded"
            source_path = $PayloadApk
            source_sha256 = $sourceSha256
            upload_id = $upload.id
            upload_sha256 = $upload.sha256
            config_path = "assets/vm_config.json"
        }
        if ($null -ne $extraMaterial) {
            $extraUploads = [Collections.Generic.List[object]]::new()
            foreach ($entry in @($extraMaterial.extra_apks)) {
                $extraPath = (Resolve-Path -LiteralPath (Join-Path $ExtraApkMaterials $entry.file)).Path
                $extraUpload = Invoke-Hdctl upload $extraPath | ConvertFrom-Json
                if ($extraUpload.sha256 -ne $entry.sha256 -or $null -eq $extraUpload.id) {
                    throw "Host upload identity does not match extra APK $($entry.file)"
                }
                $extraUploads.Add([ordered]@{
                    upload_id = $extraUpload.id
                    sha256 = $extraUpload.sha256
                    file = $entry.file
                    asset_path = $entry.asset_path
                    asset_sha256 = $entry.asset_sha256
                })
            }
            $spec.microdroid.payload_extra_apk_count = 2
            $spec.microdroid.extra_apks = @($extraUploads | ForEach-Object {
                [ordered]@{ upload_id = $_.upload_id; sha256 = $_.sha256 }
            })
            $payloadEvidence["extra_apks"] = @($extraUploads)
        }
    } else {
        $payloadEvidence = [ordered]@{ kind = "empty" }
    }
    $spec | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $specPath -Encoding utf8NoBOM
    $createdRecord = Invoke-Hdctl create --spec $specPath | ConvertFrom-Json
    if ($createdRecord.spec.id -ne $instanceId.ToString() -or
        $createdRecord.spec.guest_kind -ne "microdroid") {
        throw "Host returned the wrong created instance identity or guest kind"
    }
    $created = $true

    for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
        $cycleStarted = [DateTimeOffset]::UtcNow
        Invoke-Hdctl start $instanceId.ToString() --no-wait | Out-Null
        if ($finitePayloadMode) {
            $deadline = [DateTimeOffset]::UtcNow.AddSeconds($ReadyTimeoutSeconds)
            $terminal = $null
            do {
                $candidate = Get-Instance
                $expectedObserved = if ($ExpectedPayloadExitCode -eq 0) { "stopped" } else { "failed" }
                if ($candidate.status.observed -eq $expectedObserved) {
                    $terminal = $candidate
                    break
                }
                if ($candidate.status.observed -in @("blocked", "failed", "deleted") -and
                    $candidate.status.observed -ne $expectedObserved) {
                    throw "finite Microdroid entered unexpected terminal state $($candidate.status.observed): $($candidate.status.error_code): $($candidate.status.reason)"
                }
                Start-Sleep -Milliseconds 100
            } while ([DateTimeOffset]::UtcNow -lt $deadline)
            if ($null -eq $terminal) {
                throw "finite Microdroid did not reach $expectedObserved within $ReadyTimeoutSeconds seconds"
            }

            $runParent = Join-Path $DataRoot "runs\$instanceId"
            $runDirectories = if (Test-Path -LiteralPath $runParent -PathType Container) {
                @(Get-ChildItem -LiteralPath $runParent -Directory -Force)
            } else { @() }
            if ($runDirectories.Count -ne 1) {
                throw "finite Microdroid produced $($runDirectories.Count) run directories instead of exactly one"
            }
            $runId = [Guid]::Parse($runDirectories[0].Name)
            $runRoot = $runDirectories[0].FullName
            $manifestPath = Join-Path $runRoot "manifest.json"
            $stdoutPath = Join-Path $runRoot "microdroid.stdout.log"
            $stderrPath = Join-Path $runRoot "microdroid.stderr.log"
            $consolePath = Join-Path $runRoot "microdroid-console.txt"
            $guestLogPath = Join-Path $runRoot "microdroid-guest.log"
            $resultPath = Join-Path $runRoot "result.json"
            foreach ($path in @(
                $manifestPath, $stdoutPath, $stderrPath, $consolePath, $guestLogPath, $resultPath
            )) {
                if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
                    throw "finite Microdroid run evidence is missing: $path"
                }
            }
            $stdout = Get-Content -Raw -LiteralPath $stdoutPath
            $stderr = Get-Content -Raw -LiteralPath $stderrPath
            $guestLog = Get-Content -Raw -LiteralPath $guestLogPath
            $expectedFinishLine = "payload finished with exit code $ExpectedPayloadExitCode"
            $finishLines = @($stderr -split "`r?`n" | Where-Object {
                $_.StartsWith("payload finished with exit code ")
            })
            $shutdownLines = @($stdout -split "`r?`n" | Where-Object { $_ -eq "VM ended: Shutdown" })
            if ($finishLines.Count -ne 1 -or $finishLines[0] -ne $expectedFinishLine) {
                throw "finite Microdroid launcher did not report exactly '$expectedFinishLine'"
            }
            if ($shutdownLines.Count -ne 1) {
                throw "finite Microdroid launcher did not report exactly one VM ended: Shutdown"
            }
            if (-not $guestLog.Contains("notifying payload finished")) {
                throw "finite Microdroid Guest did not record its payload-finished notification"
            }

            $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
            $arguments = @($manifest.launch.arguments)
            foreach ($requiredArgument in @(
                "run-app", "--config-path", "assets/vm_config.json", "--debug", "full"
            )) {
                if ($requiredArgument -notin $arguments) {
                    throw "finite Payload manifest is missing required argument: $requiredArgument"
                }
            }
            $result = Get-Content -Raw -LiteralPath $resultPath | ConvertFrom-Json
            $expectedFinalState = if ($ExpectedPayloadExitCode -eq 0) { "stopped" } else { "failed" }
            $expectedErrorCode = if ($ExpectedPayloadExitCode -eq 0) { $null } else { "microdroid_payload_failed" }
            if ($result.instance_id -ne $instanceId.ToString() -or
                $result.run_id -ne $runId.ToString() -or
                $result.final_state -ne $expectedFinalState -or
                $result.exit_code -ne $ExpectedPayloadExitCode -or
                $result.error_code -ne $expectedErrorCode) {
                throw "finite Microdroid typed run result does not match exit code $ExpectedPayloadExitCode"
            }
            if ($ExpectedPayloadExitCode -eq 0) {
                if ($terminal.status.desired -ne "stopped" -or $null -ne $terminal.status.error_code) {
                    throw "successful finite Payload did not converge desired/observed state to clean Stopped"
                }
            } elseif ($terminal.status.error_code -ne "microdroid_payload_failed" -or
                $null -eq $terminal.status.reason -or
                -not $terminal.status.reason.Contains("$ExpectedPayloadExitCode")) {
                throw "nonzero finite Payload did not preserve its typed error code and original exit code"
            }
            $remainingVmProcesses = @(Get-ActiveRuntimeProcesses | Where-Object {
                $_.Name -in @("vm.exe", "virtmgr.exe", "crosvm.exe")
            })
            if ($remainingVmProcesses.Count -ne 0) {
                throw "finite Microdroid retained VM processes after natural completion"
            }

            $cycleOutput = Join-Path $outputRoot "cycles\cycle-$cycle"
            New-Item -ItemType Directory -Path $cycleOutput -Force | Out-Null
            Copy-Item -LiteralPath @(
                $manifestPath, $stdoutPath, $stderrPath, $consolePath, $guestLogPath, $resultPath
            ) -Destination $cycleOutput
            $cycleEvidence.Add([ordered]@{
                cycle = $cycle
                run_id = $runId
                ready_elapsed_millis = $null
                completion_elapsed_millis = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
                guest_cid = $manifest.launch.guest_cid
                payload_kind = $payloadMode
                expected_payload_exit_code = [int]$ExpectedPayloadExitCode
                observed_payload_exit_code = [int]$result.exit_code
                final_state = $result.final_state
                error_code = $result.error_code
                natural_shutdown = $true
                remaining_vm_process_count = 0
            })
            continue
        }
        $ready = Wait-ForObservedState -Expected "ready" -TimeoutSeconds $ReadyTimeoutSeconds
        if ($null -eq $ready.active_run_id) {
            throw "Ready Microdroid has no active run identity"
        }
        $adbEvidence = if ($AdbMode -eq "loopback") {
            $adbReady = Wait-ForAdbReady -TimeoutSeconds 35
            $state = Invoke-Adb -Arguments @("-s", $adbReady.adb_serial, "get-state")
            if ($state -ne "device") {
                throw "Microdroid ADB transport returned $state instead of device"
            }
            $sdk = Invoke-Adb -Arguments @(
                "-s", $adbReady.adb_serial, "shell", "getprop", "ro.build.version.sdk"
            )
            if ($sdk -ne "35") {
                throw "Microdroid ADB shell returned SDK $sdk instead of 35"
            }
            [ordered]@{
                mode = "loopback"
                serial = $adbReady.adb_serial
                state = $state
                sdk = [int]$sdk
                ready_elapsed_millis = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
            }
        } else {
            if ($null -ne $ready.adb_serial -or $ready.adb_ready) {
                throw "ADB-disabled Microdroid published an ADB endpoint"
            }
            [ordered]@{ mode = "disabled"; serial = $null; state = $null; sdk = $null; ready_elapsed_millis = $null }
        }

        $runRoot = Join-Path $DataRoot "runs\$instanceId\$($ready.active_run_id)"
        $manifestPath = Join-Path $runRoot "manifest.json"
        $consolePath = Join-Path $runRoot "microdroid-console.txt"
        $guestLogPath = Join-Path $runRoot "microdroid-guest.log"
        $stdoutPath = Join-Path $runRoot "microdroid.stdout.log"
        $stderrPath = Join-Path $runRoot "microdroid.stderr.log"
        foreach ($path in @($manifestPath, $consolePath, $guestLogPath, $stdoutPath, $stderrPath)) {
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
                throw "required Microdroid run evidence is missing: $path"
            }
        }
        $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
        $arguments = @($manifest.launch.arguments)
        $payloadCommand = if ($payloadMode -eq "uploaded") { "run-app" } else { "run-microdroid" }
        foreach ($requiredArgument in @($payloadCommand)) {
            if ($requiredArgument -notin $arguments) {
                throw "launch manifest is missing required argument: $requiredArgument"
            }
        }
        Assert-LaunchArgumentValue -LaunchArguments $arguments -Name "--debug" -Expected $DebugLevel
        Assert-LaunchArgumentValue -LaunchArguments $arguments -Name "--cpu-topology" -Expected $CpuTopology
        Assert-LaunchArgumentValue -LaunchArguments $arguments -Name "--mem" -Expected $MemoryMiB.ToString()
        if ($payloadMode -eq "uploaded") {
            $managedUpload = Join-Path $DataRoot "uploads\$($payloadEvidence.upload_id).apk"
            if ($managedUpload -notin $arguments -or "--config-path" -notin $arguments -or
                "assets/vm_config.json" -notin $arguments) {
                throw "run-app manifest is not bound to the managed upload and config path"
            }
            if ((Get-FileHash -LiteralPath $managedUpload -Algorithm SHA256).Hash.ToLowerInvariant() -ne
                $payloadEvidence.source_sha256) {
                throw "managed Payload digest changed after upload"
            }
            $idsigArguments = @($arguments | Where-Object { $_ -match "(?i)payload\.idsig$" })
            if ($idsigArguments.Count -ne 1 -or
                -not (Test-Path -LiteralPath $idsigArguments[0] -PathType Leaf)) {
                throw "run-app did not publish one real Payload idsig"
            }
        }
        $extraApkEvidence = $null
        if ($null -ne $extraMaterial) {
            $adbRoot = Invoke-Adb -TimeoutSeconds 15 -Arguments @("-s", $adbEvidence.serial, "root")
            $adbRootDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
            $adbRootReady = $false
            do {
                try {
                    if ((Invoke-Adb -Arguments @("-s", $adbEvidence.serial, "get-state")) -eq "device") {
                        $adbRootReady = $true
                        break
                    }
                } catch { }
                Start-Sleep -Milliseconds 200
            } while ([DateTimeOffset]::UtcNow -lt $adbRootDeadline)
            if (-not $adbRootReady) {
                throw "Full-debug Microdroid ADB did not reconnect after adb root"
            }
            $overrideCount = @($arguments | Where-Object { $_ -eq "--extra-apk-override" }).Count
            $extraIdsigCount = @($arguments | Where-Object { $_ -eq "--extra-idsig" }).Count
            if ($overrideCount -ne 2 -or $extraIdsigCount -ne 2) {
                throw "run-app manifest did not bind exactly two extra APK/idsig descriptors"
            }
            $guestExtras = [Collections.Generic.List[object]]::new()
            for ($extraIndex = 0; $extraIndex -lt 2; $extraIndex++) {
                $entry = $payloadEvidence.extra_apks[$extraIndex]
                $managedExtra = Join-Path $DataRoot "uploads\$($entry.upload_id).apk"
                if ($managedExtra -notin $arguments) {
                    throw "run-app manifest omitted managed extra APK $extraIndex"
                }
                $asset = Invoke-Adb -Arguments @(
                    "-s", $adbEvidence.serial, "exec-out", "cat",
                    "/mnt/extra-apk/$extraIndex/$($entry.asset_path)"
                )
                $assetNormalized = $asset.Replace("`r", "").Trim()
                $expectedMarker = "HD Microdroid extra APK marker $extraIndex"
                if ($assetNormalized -ne $expectedMarker) {
                    throw "Guest extra APK $extraIndex asset content or order changed: '$assetNormalized'"
                }
                $verityRoot = $null
                $mapperDevice = $null
                try {
                    $dmTable = Invoke-Adb -Arguments @(
                        "-s", $adbEvidence.serial, "shell", "dmctl", "table", "extra-apk-$extraIndex"
                    )
                    $roots = @([regex]::Matches($dmTable, '(?i)(?<![0-9a-f])[0-9a-f]{64}(?![0-9a-f])') |
                        ForEach-Object { $_.Value.ToLowerInvariant() } | Select-Object -Unique)
                    if (-not $dmTable.Contains("verity") -or $roots.Count -lt 1) {
                        throw "dmctl did not expose a verity root"
                    }
                    $verityRoot = $roots[-1]
                } catch {
                    $mapperDevice = Invoke-Adb -Arguments @(
                        "-s", $adbEvidence.serial, "shell",
                        "for d in /sys/block/dm-*; do n=`$(cat `"`$d/dm/name`" 2>/dev/null); if [ `"`$n`" = `"extra-apk-$extraIndex`" ]; then basename `"`$d`"; fi; done"
                    )
                    if ($mapperDevice -notmatch '^dm-[0-9]+$') {
                        throw "Guest extra APK $extraIndex has no named device-mapper node"
                    }
                }
                $guestExtras.Add([ordered]@{
                    index = $extraIndex
                    upload_id = $entry.upload_id
                    sha256 = $entry.sha256
                    asset_path = $entry.asset_path
                    marker = $assetNormalized
                    verity_root_sha256 = $verityRoot
                    mapper_device = $mapperDevice
                    adb_root = $adbRoot
                })
            }
            if ($null -ne $guestExtras[0].verity_root_sha256 -and
                $guestExtras[0].verity_root_sha256 -eq $guestExtras[1].verity_root_sha256) {
                throw "two extra APKs unexpectedly reused one verity identity"
            }
            $extraApkEvidence = @($guestExtras)
        }
        if ($manifest.instance.guest_kind -ne "microdroid" -or
            $manifest.launch.guest_cid -le 2 -or
            $manifest.launch.executable -notmatch "(?i)vm\.exe$") {
            throw "launch manifest does not describe a valid Windows Microdroid run"
        }

        $guestLog = [string](Get-Content -Raw -LiteralPath $guestLogPath)
        $consoleBytes = (Get-Item -LiteralPath $consolePath).Length
        $guestLogBytes = (Get-Item -LiteralPath $guestLogPath).Length
        if ($DebugLevel -eq "full") {
            foreach ($marker in @(
                "payload verification successful",
                "boot completed, time to run payload",
                "Notified host payload ready successfully"
            )) {
                if (-not $guestLog.Contains($marker)) {
                    throw "Guest log is missing readiness marker: $marker"
                }
            }
            if ($consoleBytes -lt 1024 -or $guestLogBytes -lt 512) {
                throw "Full-debug Microdroid console or Guest log is unexpectedly small"
            }
        } else {
            if ($consoleBytes -ne 0 -or $guestLogBytes -ne 0) {
                throw "None-debug Microdroid unexpectedly exposed debug console or Guest logs"
            }
            $readyMillis = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
            if ($readyMillis -gt 10000) {
                throw "None-debug Microdroid exceeded the 10-second Ready budget: $readyMillis ms"
            }
        }

        $storageEvidence = $null
        $storagePath = Join-Path $DataRoot "instances\$instanceId\microdroid\storage.img"
        if ($EncryptedStorageMiB -eq 0) {
            if ("--storage" -in $arguments -or (Test-Path -LiteralPath $storagePath)) {
                throw "storage-disabled Microdroid unexpectedly published encrypted storage"
            }
        } else {
            $storageBytes = [long]$EncryptedStorageMiB * 1024 * 1024
            Assert-LaunchArgumentValue -LaunchArguments $arguments -Name "--storage" -Expected $storagePath
            Assert-LaunchArgumentValue -LaunchArguments $arguments -Name "--storage-size" -Expected $storageBytes.ToString()
            $storageFile = Get-Item -LiteralPath $storagePath -ErrorAction Stop
            if ($storageFile.Length -ne $storageBytes) {
                throw "encrypted storage length $($storageFile.Length) does not match $storageBytes"
            }
            $storageUuid = $null
            $formatted = $null
            if ($DebugLevel -eq "full") {
                $console = [string](Get-Content -Raw -LiteralPath $consolePath)
                $uuidMatches = @([regex]::Matches(
                    $console,
                    'EXT4-fs \(dm-[0-9]+\): mounted filesystem ([0-9a-f-]{36}) r/w',
                    [Text.RegularExpressions.RegexOptions]::IgnoreCase
                ) | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() } | Select-Object -Unique)
                if ($uuidMatches.Count -ne 1) {
                    throw "Guest console must expose exactly one encrypted-store ext4 UUID"
                }
                $storageUuid = $uuidMatches[0]
                $formatted = $console.Contains("Creating filesystem with")
                if (($cycle -eq 1) -ne $formatted) {
                    throw "encrypted storage must format exactly once on the first cycle"
                }
            }
            $storageEvidence = [ordered]@{
                path = $storageFile.FullName
                bytes = $storageFile.Length
                creation_time_utc = $storageFile.CreationTimeUtc.ToString("o")
                sha256 = $null
                ext4_uuid = $storageUuid
                formatted = $formatted
            }
            if ($cycleEvidence.Count -gt 0) {
                $firstStorage = $cycleEvidence[0].encrypted_storage
                if ($storageEvidence.path -ne $firstStorage.path -or
                    $storageEvidence.creation_time_utc -ne $firstStorage.creation_time_utc -or
                    ($null -ne $storageEvidence.ext4_uuid -and
                        $storageEvidence.ext4_uuid -ne $firstStorage.ext4_uuid)) {
                    throw "encrypted storage Host identity or Guest ext4 UUID changed across restart"
                }
            }
        }

        $cycleOutput = Join-Path $outputRoot "cycles\cycle-$cycle"
        New-Item -ItemType Directory -Path $cycleOutput -Force | Out-Null
        Copy-Item -LiteralPath $manifestPath, $consolePath, $guestLogPath, $stdoutPath, $stderrPath -Destination $cycleOutput

        $runtimeProcesses = Get-ActiveRuntimeProcesses
        foreach ($requiredProcess in @("hd-host.exe", "hd-worker.exe", "vm.exe", "virtmgr.exe", "crosvm.exe")) {
            if (-not ($runtimeProcesses | Where-Object Name -eq $requiredProcess)) {
                throw "Ready Microdroid process topology is missing $requiredProcess"
            }
        }

        if ($AdbMode -eq "loopback") {
            Invoke-Hdctl stop $instanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
        } else {
            Invoke-Hdctl stop $instanceId.ToString() --force --no-wait | Out-Null
        }
        $stopped = Wait-ForObservedState -Expected "stopped" -TimeoutSeconds 30
        if ($null -ne $stopped.active_run_id -or $null -ne $stopped.adb_serial) {
            throw "stopped Microdroid retained an active run or ADB endpoint"
        }
        if ($null -ne $storageEvidence) {
            $storageEvidence["sha256"] = (Get-FileHash -LiteralPath $storagePath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        if ($AdbMode -eq "loopback") {
            $consoleAfterStop = Get-Content -Raw -LiteralPath $consolePath
            if (-not $consoleAfterStop.Contains("reboot: Power down")) {
                throw "Full-debug Microdroid did not record Guest power down during graceful stop"
            }
            Copy-Item -LiteralPath $consolePath -Destination $cycleOutput -Force
        } elseif ($DebugLevel -eq "none") {
            Copy-Item -LiteralPath $stdoutPath, $stderrPath -Destination $cycleOutput -Force
        }
        $resultPath = Join-Path $runRoot "result.json"
        $result = Get-Content -Raw -LiteralPath $resultPath | ConvertFrom-Json
        if ($result.final_state -ne "stopped" -or $null -ne $result.error_code) {
            throw "Microdroid run result did not converge to a clean stopped state"
        }
        Copy-Item -LiteralPath $resultPath -Destination $cycleOutput
        if ($null -ne $extraMaterial) {
            foreach ($extraIndex in 0..1) {
                if (Test-Path -LiteralPath (Join-Path $runRoot "microdroid-extra-$extraIndex.idsig")) {
                    throw "finished run retained reproducible extra idsig $extraIndex"
                }
            }
        }

        $cycleEvidence.Add([ordered]@{
            cycle = $cycle
            run_id = $ready.active_run_id
            ready_elapsed_millis = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
            guest_cid = $manifest.launch.guest_cid
            console_bytes = $consoleBytes
            guest_log_bytes = $guestLogBytes
            payload_kind = $payloadMode
            debug_level = $DebugLevel
            cpu_topology = $CpuTopology
            memory_mib = $MemoryMiB
            encrypted_storage = $storageEvidence
            extra_apks = $extraApkEvidence
            adb = $adbEvidence
            runtime_processes = @($runtimeProcesses | Sort-Object Name | ForEach-Object {
                [ordered]@{ name = $_.Name; pid = $_.ProcessId; parent_pid = $_.ParentProcessId }
            })
            final_state = $result.final_state
        })
    }
    if ($null -ne $extraMaterial -and $cycleEvidence.Count -gt 1) {
        for ($extraIndex = 0; $extraIndex -lt 2; $extraIndex++) {
            $expectedRoot = $cycleEvidence[0].extra_apks[$extraIndex].verity_root_sha256
            foreach ($cycle in @($cycleEvidence | Select-Object -Skip 1)) {
                if ($null -ne $expectedRoot -and
                    $cycle.extra_apks[$extraIndex].verity_root_sha256 -ne $expectedRoot) {
                    throw "extra APK $extraIndex verity identity changed across restart"
                }
            }
        }
    }
} catch {
    $failure = $_.Exception.Message
    $failureStack = $_.ScriptStackTrace
} finally {
    if ($null -ne $failure) {
        try {
            Save-FailureRunEvidence
        } catch {
            $failure = "$failure; failure evidence capture failed: $($_.Exception.Message)"
        }
    }
    if ($created) {
        try {
            $current = Get-Instance
            if ($current.status.observed -notin @("defined", "stopped", "deleted")) {
                Invoke-Hdctl stop $instanceId.ToString() --force | Out-Null
            }
            if ($current.status.observed -ne "deleted") {
                Invoke-Hdctl delete $instanceId.ToString() | Out-Null
            }
        } catch {
            if ($null -eq $failure) { $failure = "cleanup failed: $($_.Exception.Message)" }
        }
    }
    try {
        Invoke-Hdctl shutdown --stop-all | Out-Null
    } catch {
        if ($null -eq $failure) { $failure = "Host shutdown failed: $($_.Exception.Message)" }
    }
    if ($null -ne $hostProcess) {
        try {
            [void]$hostProcess.WaitForExit(10000)
        } catch {
            if ($null -eq $failure) { $failure = "Host did not exit after shutdown" }
        }
        if (-not $hostProcess.HasExited) {
            Stop-Process -Id $hostProcess.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Start-Sleep -Milliseconds 250
}

$remaining = @(Get-ScopedRuntimeProcesses)
if ($remaining.Count -ne 0 -and $null -eq $failure) {
    $failure = "isolated Windows Microdroid processes remain after shutdown: $($remaining.Name -join ', ')"
}
$evidence = [ordered]@{
    schema_version = 1
    status = if ($null -eq $failure) { "pass" } else { "fail" }
    platform = "windows-x86_64-gnu"
    instance_id = $instanceId
    started_at = $startedAt.ToString("o")
    finished_at = [DateTimeOffset]::UtcNow.ToString("o")
    artifact_root = (Resolve-Path -LiteralPath $ArtifactRoot).Path
    dist_root = (Resolve-Path -LiteralPath $DistRoot).Path
    data_root = (Resolve-Path -LiteralPath $DataRoot).Path
    payload = $payloadEvidence
    adb_mode = $AdbMode
    debug_level = $DebugLevel
    cpu_topology = $CpuTopology
    memory_mib = $MemoryMiB
    encrypted_storage_mib = if ($EncryptedStorageMiB -eq 0) { $null } else { $EncryptedStorageMiB }
    expected_payload_exit_code = if ($finitePayloadMode) { [int]$ExpectedPayloadExitCode } else { $null }
    cycles_requested = $Cycles
    cycles_completed = $cycleEvidence.Count
    cycles = $cycleEvidence
    host_pid = if ($null -ne $hostProcess) { $hostProcess.Id } else { $null }
    host_stdout = $hostStdout
    host_stderr = $hostStderr
    remaining_process_count = $remaining.Count
    failure = $failure
    failure_stack = $failureStack
}
$evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
if ($null -ne $failure) {
    throw "$failure (evidence: $evidencePath)"
}
Write-Output $evidencePath
